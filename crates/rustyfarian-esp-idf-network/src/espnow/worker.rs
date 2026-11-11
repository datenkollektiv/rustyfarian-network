//! The `espnow-tx` worker adapter: the ESP-IDF port of the shared transmit worker (ADR 020,
//! ADR 022, ADR 023).
//!
//! Every ESP-NOW transmission of an [`EspIdfEspNow`](super::EspIdfEspNow) runs on one thread.
//! The loop and every decision behind it (the queue, ids, deadlines, cancellation, the
//! scan-versus-send exclusion, the scan drive and its restore, the shutdown order) live in
//! [`juggler::espnow::EspNowWorker`] with host tests against a mock radio; this module is the
//! thin ESP-IDF side, like `IdfRadio` is for the Wi-Fi `LinkSupervisor`:
//!
//! - [`IdfRadio`] implements [`EspNowRadio`] with one `esp_now_*` / `esp_wifi_*` call per method.
//!   The promiscuous bracket of an unassociated-station send is a fact of this chip and stays
//!   in [`send_frame`].
//! - The send callback runs in the Wi-Fi task and is registered by the driver; it only feeds
//!   [`SendEvents::send_done`](juggler::espnow::SendEvents::send_done).
//! - [`spawn`] names the FreeRTOS task (`ThreadNaming`) and starts the thread with a fixed
//!   stack ([`WORKER_STACK_SIZE`]).
//! - The error mapping from the worker's errors to [`EspIdfEspNowError`] lives here.
//!
//! # Blocking forms
//!
//! `EspNowDriver::send`, `send_and_wait` and `scan_for_peer_blocking` park the calling thread
//! until the worker has served them.
//! They must not run on a thread that also polls [`EspNowLink`](juggler::espnow::EspNowLink)
//! futures of the same driver: futures that were polled once and are then left unpolled on that
//! thread keep their queue slots ([`TX_SLOTS`](juggler::espnow::TX_SLOTS)) reserved, and a
//! blocking call on the same thread can then wait for a slot that only that thread could free.
//! Plain `send` bounds this with [`SEND_QUEUE_DEADLINE`](super::SEND_QUEUE_DEADLINE) (it fails
//! with `Timeout`), while `send_and_wait` is bounded by its own timeout; a scan has no such bound.
//!
//! # Not covered by host tests
//!
//! This module needs ESP-IDF and cannot run on the host, so its `#[cfg(test)]` tests only
//! type-check (the workspace clippy compiles them) and cover error mapping.
//! The decisions are tested on the host in `juggler::espnow::worker`; the behaviour of the
//! threads and the callback ordering on the chip is exercised by the C3 smoke run only.

use std::ffi::CStr;
use std::future::Future;
use std::pin::pin;
use std::sync::Arc;
use std::task::{Context, Poll, Wake, Waker};
use std::thread::Thread;
use std::time::Duration;

use anyhow::Context as _;
use esp_idf_svc::sys::{esp, EspError};
use juggler::espnow::{
    EspNowRadio, EspNowWorker, MacAddress, PeerConfig, ScanConfig, ScanError, ScanResult, SendKind,
    WorkerConfig,
};
use juggler::wifi::TxPowerLevel;

use super::{EspIdfEspNowError, RadioMode};

/// Stack of the `espnow-tx` thread in bytes.
///
/// A starting value: the thread holds a frame copy (250 B), a scan machine, one probe call and
/// log formatting.
/// Measure the high-water mark on hardware (`espnow-tx` task) before shrinking it, and measure it
/// including a scan whose peer registration fails: that path builds an error and logs the cause,
/// and is the deepest on this thread.
pub(super) const WORKER_STACK_SIZE: usize = 6144;

/// The ESP-IDF flavour of the shared transmit worker.
pub(super) type Worker = EspNowWorker<IdfRadio>;

/// Result of one transmission, as reported to the caller of the frame.
pub(super) type TxOutcome = juggler::espnow::TxOutcome<EspError>;

/// The ESP-NOW and Wi-Fi calls of the ESP-IDF driver; the worker thread is the only caller.
pub(super) struct IdfRadio;

impl EspNowRadio for IdfRadio {
    type Error = EspError;

    fn send(&mut self, mac: &MacAddress, data: &[u8], repin: Option<u8>) -> Result<(), EspError> {
        esp!(send_frame(mac, data, repin))
    }

    fn set_channel(&mut self, channel: u8) -> Result<(), EspError> {
        // SAFETY: esp_wifi_set_channel is an FFI call into the ESP-IDF Wi-Fi subsystem, which is
        // initialised by init_with_radio().  The worker calls it only for scans, which are
        // admitted on a driver-owned radio, so the radio is owned and running; in SoftAP mode
        // the change is a CSA with no connected stations and takes effect immediately.  No
        // frame is on the air: the worker changes the channel only between probes it has
        // waited out.
        esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_set_channel(
                channel,
                esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
            )
        })
    }

    fn tx_power(&mut self) -> Result<i8, EspError> {
        let mut level: i8 = 0;
        // SAFETY: esp_wifi_get_max_tx_power is an FFI call into the ESP-IDF Wi-Fi subsystem,
        // which is initialised by init_with_radio().  It writes the current TX power to the
        // &mut i8 we pass in.
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_get_max_tx_power(&mut level) })?;
        Ok(level)
    }

    fn set_tx_power(&mut self, quarter_dbm: i8) -> Result<(), EspError> {
        // SAFETY: esp_wifi_set_max_tx_power is an FFI call into the ESP-IDF Wi-Fi subsystem,
        // which is initialised by init_with_radio().  The worker passes either the level of
        // TxPowerLevel::Max, within the valid [8, 84] quarter-dBm range, or a value read back
        // from esp_wifi_get_max_tx_power.
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(quarter_dbm) })
    }

    fn add_peer(&mut self, peer: &PeerConfig) -> Result<(), EspError> {
        super::peer_add_raw(peer)
    }

    fn remove_peer(&mut self, mac: &MacAddress) -> Result<(), EspError> {
        super::peer_del_raw(mac)
    }
}

/// The driver error for an `esp_now_send` that refused the frame.
fn send_failure(error: EspError) -> EspIdfEspNowError {
    EspIdfEspNowError::Driver(anyhow::Error::new(error).context("failed to send ESP-NOW frame"))
}

/// Maps the worker's error onto the public contract; the scan shapes are those of the
/// pre-state-machine implementation: a context message over the driver error for registration
/// failures, the plain verdict otherwise.
fn map_error(error: juggler::espnow::WorkerError<EspError>) -> EspIdfEspNowError {
    use juggler::espnow::WorkerError as W;
    match error {
        W::Timeout => EspIdfEspNowError::Timeout,
        W::ScanInProgress => EspIdfEspNowError::ScanInProgress,
        W::Shutdown => EspIdfEspNowError::Shutdown,
        W::Busy => EspIdfEspNowError::Busy,
        W::PayloadTooLarge { len } => EspIdfEspNowError::PayloadTooLarge { len },
        W::Broadcast => EspIdfEspNowError::Broadcast,
        W::RadioNotOwned => EspIdfEspNowError::RadioNotOwned,
        W::Scan {
            kind: ScanError::RegisterTempPeer,
            cause: Some(cause),
        } => EspIdfEspNowError::Driver(
            anyhow::Error::new(cause)
                .context("failed to add ESP-NOW peer")
                .context("failed to register temporary peer for scanning"),
        ),
        W::Scan {
            kind: ScanError::RegisterFoundPeer { .. },
            cause: Some(cause),
        } => EspIdfEspNowError::Driver(
            anyhow::Error::new(cause)
                .context("failed to add ESP-NOW peer")
                .context("failed to re-register peer on discovered channel"),
        ),
        W::Scan { kind, .. } => EspIdfEspNowError::Scan(kind),
    }
}

/// Queues one frame for the worker and waits for its result.
///
/// See [`EspNowWorker::run_frame`]; the error is mapped to the public contract.
pub(super) async fn run_frame(
    worker: &Worker,
    mac: &MacAddress,
    data: &[u8],
    timeout: Option<Duration>,
    kind: SendKind,
) -> Result<TxOutcome, EspIdfEspNowError> {
    worker
        .run_frame(mac, data, timeout, kind)
        .await
        .map_err(map_error)
}

/// Runs a scan on the worker; see [`EspNowWorker::scan_for_peer`].
pub(super) async fn scan_for_peer(
    worker: &Worker,
    mac: &MacAddress,
    config: &ScanConfig<'_>,
) -> Result<ScanResult, EspIdfEspNowError> {
    worker.scan_for_peer(mac, config).await.map_err(map_error)
}

/// `Ok` only for a MAC-layer ACK ([`EspIdfEspNow::send_acked`](super::EspIdfEspNow::send_acked)).
pub(super) fn require_ack(outcome: TxOutcome) -> Result<(), EspIdfEspNowError> {
    match outcome {
        TxOutcome::Acked => Ok(()),
        TxOutcome::NoAck => Err(EspIdfEspNowError::NoAck),
        TxOutcome::SendFailed(error) => Err(send_failure(error)),
    }
}

/// `Ok` once the frame left the radio, acknowledged or not (plain `EspNowDriver::send`).
pub(super) fn ignore_ack(outcome: TxOutcome) -> Result<(), EspIdfEspNowError> {
    match outcome {
        TxOutcome::Acked | TxOutcome::NoAck => Ok(()),
        TxOutcome::SendFailed(error) => Err(send_failure(error)),
    }
}

// ---------------------------------------------------------------------------------------------
// Blocking forms
// ---------------------------------------------------------------------------------------------

/// Wakes a parked thread; the waker of [`block_on`].
struct Unparker(Thread);

impl Wake for Unparker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on the calling thread, parking between polls.
pub(super) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unparker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

// ---------------------------------------------------------------------------------------------
// ESP-IDF facts
// ---------------------------------------------------------------------------------------------

/// The primary channel the Wi-Fi radio is on (`esp_wifi_get_channel`), `None` when the driver
/// refuses the query (radio not initialised or not started).
///
/// Any thread may call it; the function only reads the driver's state.
pub(super) fn current_channel() -> Option<u8> {
    let mut primary: u8 = 0;
    let mut second = esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE;
    // SAFETY: plain FFI call into the ESP-IDF Wi-Fi subsystem with pointers to two live locals
    // it writes; it reads driver state only and fails cleanly before the driver is initialised.
    let ret = unsafe { esp_idf_svc::sys::esp_wifi_get_channel(&mut primary, &mut second) };
    if ret != esp_idf_svc::sys::ESP_OK {
        log::debug!("esp_wifi_get_channel failed: {ret:#x}");
        return None;
    }
    Some(primary)
}

/// Hands one frame to the radio and returns the `esp_err_t`.
///
/// With `repin`, the radio is first re-pinned to that channel inside a promiscuous bracket
/// (unassociated STA mode, ADR 012).
///
/// `set_promiscuous(false)` and `esp_now_send` must be consecutive instructions to minimise the
/// scheduler window in which the Wi-Fi background scan task (priority 23) can preempt a
/// lower-priority thread and drift the channel.
/// Even with the tightest possible bracket the race is not fully eliminated: see ADR 012 for the
/// ~0-20 % `ESP_ERR_ESPNOW_CHAN` rate this path exhibits in production.
fn send_frame(mac: &MacAddress, data: &[u8], repin: Option<u8>) -> i32 {
    let Some(channel) = repin else {
        // SAFETY: plain FFI call; `mac` points to 6 bytes and `data` to `data.len()` bytes, both
        // valid for the duration of the call (ESP-NOW copies the payload before returning).
        return unsafe { esp_idf_svc::sys::esp_now_send(mac.as_ptr(), data.as_ptr(), data.len()) };
    };
    // SAFETY: repin is only set for RadioMode::OwnedStaPromisc, so the radio is owned and
    // running. `channel` was written by a successful scan, so it is a valid 2.4 GHz channel.
    // `mac` and `data` outlive the call.
    //
    // The tight bracket captures every intermediate `esp_err_t` so that a channel-race or
    // country-code failure surfaces as a specific log line rather than an opaque "peer did not
    // ACK" downstream.
    let (promisc_on_ret, set_ch_ret, promisc_off_ret, send_ret) = unsafe {
        let on = esp_idf_svc::sys::esp_wifi_set_promiscuous(true);
        let set = esp_idf_svc::sys::esp_wifi_set_channel(
            channel,
            esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
        );
        let off = esp_idf_svc::sys::esp_wifi_set_promiscuous(false);
        let send = esp_idf_svc::sys::esp_now_send(mac.as_ptr(), data.as_ptr(), data.len());
        (on, set, off, send)
    };

    if promisc_on_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_promiscuous(true) failed: {:#x} \
             (channel re-pin skipped, send proceeded on previous channel)",
            promisc_on_ret
        );
    }
    if set_ch_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_channel({}) failed: {:#x} \
             (frame sent on whatever channel the radio was on)",
            channel,
            set_ch_ret
        );
    }
    if promisc_off_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_promiscuous(false) failed: {:#x} \
             (radio left in promiscuous mode)",
            promisc_off_ret
        );
    }
    send_ret
}

/// Labels the pthreads spawned from the calling thread while it lives, because
/// `std::thread::Builder::name` does not reach FreeRTOS on ESP-IDF (`docs/project-lore.md`,
/// "ESP-IDF FreeRTOS Tasks & Threads").
///
/// Rust passes its own stack size to `pthread_create`, which overrides the configured one, so
/// this only sets the FreeRTOS task name; it must stay at most 15 characters.
/// Dropping the guard restores the pthread configuration the caller had, exactly; a caller that
/// had none gets a default configuration back (ESP-IDF cannot clear one), which creates threads
/// like no configuration does.
struct ThreadNaming {
    /// The caller's configuration, `None` when it had none.
    saved: Option<esp_idf_svc::sys::esp_pthread_cfg_t>,
    /// False when the naming could not be set up: nothing to restore.
    active: bool,
}

impl ThreadNaming {
    fn set(name: &'static CStr) -> Self {
        use esp_idf_svc::sys::{
            esp_pthread_cfg_t, esp_pthread_get_cfg, esp_pthread_get_default_config,
            esp_pthread_set_cfg, ESP_ERR_NOT_FOUND, ESP_OK,
        };
        // SAFETY: plain FFI calls with pointers to live locals; `name` is a 'static C string, as
        // ESP-IDF keeps the pointer.
        unsafe {
            let mut current = esp_pthread_get_default_config();
            let got = esp_pthread_get_cfg(&mut current);
            let saved = match got {
                ESP_OK => Some(current),
                ESP_ERR_NOT_FOUND => None,
                code => {
                    log::warn!(
                        "esp_pthread_get_cfg failed: {code:#x}; the espnow-tx thread keeps its \
                         default FreeRTOS name"
                    );
                    return Self {
                        saved: None,
                        active: false,
                    };
                }
            };
            let mut cfg: esp_pthread_cfg_t = saved.unwrap_or(current);
            cfg.thread_name = name.as_ptr();
            cfg.inherit_cfg = false;
            let set = esp_pthread_set_cfg(&cfg);
            if set != ESP_OK {
                log::warn!(
                    "esp_pthread_set_cfg failed: {set:#x}; the espnow-tx thread keeps its \
                     default FreeRTOS name"
                );
            }
            Self {
                saved,
                active: set == ESP_OK,
            }
        }
    }
}

impl Drop for ThreadNaming {
    fn drop(&mut self) {
        use esp_idf_svc::sys::{esp_pthread_get_default_config, esp_pthread_set_cfg, ESP_OK};
        if !self.active {
            return;
        }
        // SAFETY: plain FFI calls with pointers to live locals; the restored configuration is
        // the caller's own, whose `thread_name` pointer it set itself.
        let restored = unsafe {
            match &self.saved {
                Some(cfg) => esp_pthread_set_cfg(cfg),
                None => esp_pthread_set_cfg(&esp_pthread_get_default_config()),
            }
        };
        if restored != ESP_OK {
            log::warn!("failed to restore the caller's pthread configuration: {restored:#x}");
        }
    }
}

/// Spawns the `espnow-tx` worker for a driver in `mode`.
///
/// `initial_pin` is the channel the owned radio starts on, if the driver set one.
///
/// # Errors
///
/// Fails if the thread cannot be created (out of memory).
pub(super) fn spawn(mode: RadioMode, initial_pin: Option<u8>) -> anyhow::Result<Worker> {
    let config = WorkerConfig {
        owns_radio: mode.owns_radio(),
        interface: mode.wifi_interface(),
        repin: matches!(mode, RadioMode::OwnedStaPromisc),
        initial_pin,
        burst_tx_power: TxPowerLevel::Max.to_quarter_dbm(),
    };
    let spawned = {
        let _naming = ThreadNaming::set(c"espnow-tx");
        EspNowWorker::spawn(
            IdfRadio,
            config,
            std::thread::Builder::new()
                .name("espnow-tx".into())
                .stack_size(WORKER_STACK_SIZE),
        )
    };
    spawned.context("failed to spawn the espnow-tx thread")
}

#[cfg(test)]
mod tests {
    use super::*;
    use juggler::espnow::WorkerError;

    #[test]
    fn worker_errors_map_to_the_contract_errors() {
        assert!(matches!(
            map_error(WorkerError::Timeout),
            EspIdfEspNowError::Timeout
        ));
        assert!(matches!(
            map_error(WorkerError::Busy),
            EspIdfEspNowError::Busy
        ));
        assert!(matches!(
            map_error(WorkerError::ScanInProgress),
            EspIdfEspNowError::ScanInProgress
        ));
        assert!(matches!(
            map_error(WorkerError::PayloadTooLarge { len: 300 }),
            EspIdfEspNowError::PayloadTooLarge { len: 300 }
        ));
        assert!(matches!(
            map_error(WorkerError::Broadcast),
            EspIdfEspNowError::Broadcast
        ));
        assert!(matches!(
            map_error(WorkerError::Shutdown),
            EspIdfEspNowError::Shutdown
        ));
        assert!(matches!(
            map_error(WorkerError::RadioNotOwned),
            EspIdfEspNowError::RadioNotOwned
        ));
        assert!(matches!(
            map_error(WorkerError::Scan {
                kind: ScanError::Aborted,
                cause: None
            }),
            EspIdfEspNowError::Scan(ScanError::Aborted)
        ));
    }

    #[test]
    fn only_an_unacked_transmission_is_no_ack() {
        assert!(require_ack(TxOutcome::Acked).is_ok());
        assert!(matches!(
            require_ack(TxOutcome::NoAck),
            Err(EspIdfEspNowError::NoAck)
        ));
        assert!(ignore_ack(TxOutcome::NoAck).is_ok());
    }
}
