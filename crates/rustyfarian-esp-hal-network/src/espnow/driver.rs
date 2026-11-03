//! Hardware half of the ESP-NOW driver: `esp-radio` glue behind [`EspHalEspNow`].

use core::cell::RefCell;
use core::future::poll_fn;
use core::task::Poll;
use core::time::Duration;

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use esp_radio::esp_now::{
    EspNow, EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface,
    PeerInfo, ReceivedData,
};
use esp_radio::wifi::{ControllerConfig, WifiController};
use juggler::espnow::{
    Completion, EspNowDriver, EspNowEvent, EspNowLink, MacAddress, Next, OversizeLog, PeerConfig,
    Refusal, ScanAction, ScanConfig, ScanInput, ScanMachine, ScanResult, SendKind, SendOutcome,
    TxCore, TxId, WifiInterface,
};
use juggler::wifi::TxPowerLevel;

use super::cell::{Gate, Lease, SharedCopy, Singleton, TakeCell, WaiterList, Wakeups};
use super::{
    completion_result, existing_peer_is_idempotent, expired, plan_peer, refusal_error,
    runner_wait_deadline, scan_refusal_error, validate_channel, validate_frame_len,
    validate_tx_power, EspHalEspNowError, EspNowConfig, PeerPlan, RadioError,
    DEFAULT_TX_POWER_QUARTER_DBM, MAX_DATA_LEN,
};

/// Frames examined per [`EspNowDriver::try_recv`] call when the head of the queue is oversize.
const RX_SKIP_LIMIT: usize = 16;

/// Pause between attempts of the sender runner to lease a sender that a sync send holds.
const RUNNER_LEASE_RETRY_MS: u64 = 1;
/// Poll interval while an async send waits for `run_sender` to be polled for the first time.
const RUNNER_START_POLL_MS: u64 = 1;
/// How long an async send waits for `run_sender` to start before it fails with `NoSenderRunner`.
const RUNNER_START_GRACE_MS: u64 = 100;
/// Poll interval while a scan waits for a sync send to hand the sender back.
const SCAN_SENDER_POLL_MS: u64 = 1;

/// Request slots of the send queue: one live application request per waiting caller up to this
/// many, plus the scan's probe and one abandoned frame still on the air.
const TX_SLOTS: usize = 4;
/// Callers that park while every slot is taken; one more re-polls itself until a slot frees.
const FULL_WAITERS: usize = 4;
/// Concurrent `receive` callers that park; one more re-polls itself until its turn.
const RECEIVE_WAITERS: usize = 4;
/// Wakers one locked operation can collect: every slot, every parked waiter, the scan gate.
const WAKE_CAPACITY: usize = TX_SLOTS + FULL_WAITERS + 2;

type TxResult = Result<(), EspHalEspNowError>;
type Wakes = Wakeups<WAKE_CAPACITY>;

/// Everything behind the one send-queue lock.
struct TxState {
    core: TxCore<TxResult, TX_SLOTS>,
    /// Callers waiting for a free slot ([`Refusal::Full`]).
    full_waiters: WaiterList<FULL_WAITERS>,
    /// The scan, waiting for the frame on the air to finish before its first radio action.
    idle_waiters: WaiterList<1>,
}

/// One frame handed out by [`TxCore::next`], copied so the lock can be released to transmit.
struct Frame {
    id: TxId,
    mac: MacAddress,
    len: usize,
    data: [u8; MAX_DATA_LEN],
}

/// One ESP-NOW instance exists per boot (`esp-radio` panics on a second one).
static INSTANCE: SharedCopy<bool> = SharedCopy::new(false);

/// ESP-NOW handle for the esp-hal tier; see the [module documentation](super).
///
/// All methods take `&self`, so the handle can live in a `StaticCell` and be shared between a
/// receive task and a send task.
pub struct EspHalEspNow {
    manager: EspNowManager,
    sender: TakeCell<EspNowSender>,
    receiver: TakeCell<EspNowReceiver>,
    /// Filled by [`EspHalEspNow::new_owned`] only.
    controller: TakeCell<WifiController<'static>>,
    owns_controller: bool,
    /// `None` until this handle sets a level (`from_controller` without a configured power).
    tx_power_quarter_dbm: SharedCopy<Option<i8>>,
    /// The send queue: request slots, ids, deadlines, cancellation and scan exclusion.
    /// One lock, held for a few instructions and never across an await or a send.
    tx: Mutex<CriticalSectionRawMutex, RefCell<TxState>>,
    /// Wakes the runner after a submission (latched, so a wake-up is never lost).
    runner_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Set while [`EspHalEspNow::run_sender`] is being polled.
    runner_active: SharedCopy<bool>,
    /// Serialises [`EspHalEspNow::receive`] callers: `esp-radio` keeps one receive waker.
    receive_gate: Gate<RECEIVE_WAITERS>,
    oversize: OversizeLog,
    /// The channel the driver keeps pinned: the configured one at start, then the last scan's.
    pinned: SharedCopy<Option<u8>>,
    /// Declared last: released after the `esp-radio` parts have been dropped.
    _instance: Singleton<'static>,
}

// The handle is shared by reference between a receive task and a send task (`StaticCell`).
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<EspHalEspNow>();
};

impl EspHalEspNow {
    /// Starts ESP-NOW on a Wi-Fi controller that the caller keeps (coordinator that also talks to
    /// a router).
    ///
    /// The station must be configured already; the radio keeps running as long as either the
    /// controller or this handle is alive.
    /// Only the TX power of `config` is applied, and only when it sets one; by default the
    /// caller's TX power is left alone (see the crate's TX-power policy for associated stations).
    /// The channel follows the access point.
    /// Build the controller with A-MPDU off (`HalWifiConfig::with_ampdu(false)`).
    /// Changing the TX power later is the caller's job on its controller.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::AlreadyInitialised`] while another handle is alive.
    /// * [`EspHalEspNowError::InvalidTxPower`] for a value outside `[8, 84]`.
    /// * [`EspHalEspNowError::Wifi`] if the driver rejects the TX power.
    pub fn from_controller(
        controller: &mut WifiController<'_>,
        config: EspNowConfig,
    ) -> Result<Self, EspHalEspNowError> {
        config.validate()?;
        let instance =
            Singleton::acquire(&INSTANCE).ok_or(EspHalEspNowError::AlreadyInitialised)?;
        if let Some(quarter_dbm) = config.tx_power_quarter_dbm() {
            controller
                .set_max_tx_power(quarter_dbm)
                .map_err(EspHalEspNowError::Wifi)?;
        }
        let esp_now = controller.esp_now();
        Ok(Self::assemble(
            esp_now,
            None,
            config.tx_power_quarter_dbm(),
            instance,
        ))
    }

    /// Starts the radio and ESP-NOW without any access point: an unassociated station that keeps
    /// its station MAC (ESP32-C3 matrix and nunchuk).
    ///
    /// Runs `WifiController::new` with A-MPDU RX/TX off; its default station configuration starts
    /// Wi-Fi without connecting; no SSID, no `embassy-net`, no executor needed.
    /// The TX power of `config` is applied, [`DEFAULT_TX_POWER_QUARTER_DBM`] (13 dBm) when unset.
    /// `esp_rtos::start` must have run and a heap must exist.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::AlreadyInitialised`] while another handle is alive.
    /// * [`EspHalEspNowError::InvalidTxPower`] / [`EspHalEspNowError::InvalidChannel`] for a bad
    ///   config.
    /// * [`EspHalEspNowError::Wifi`] if the driver fails to start.
    pub fn new_owned(
        wifi: esp_hal::peripherals::WIFI<'static>,
        config: EspNowConfig,
    ) -> Result<Self, EspHalEspNowError> {
        config.validate()?;
        let instance =
            Singleton::acquire(&INSTANCE).ok_or(EspHalEspNowError::AlreadyInitialised)?;

        // The IDF brain sdkconfig disables both; Block-Ack exchanges starve ESP-NOW reception.
        let controller_config = ControllerConfig::default()
            .with_ampdu_rx_enable(false)
            .with_ampdu_tx_enable(false);
        let mut controller =
            WifiController::new(wifi, controller_config).map_err(EspHalEspNowError::Wifi)?;
        // `WifiController::new` leaves the esp-radio default of 20 (5 dBm); always set ours.
        let tx_power = config
            .tx_power_quarter_dbm()
            .unwrap_or(DEFAULT_TX_POWER_QUARTER_DBM);
        controller
            .set_max_tx_power(tx_power)
            .map_err(EspHalEspNowError::Wifi)?;

        let esp_now = controller.esp_now();
        let handle = Self::assemble(esp_now, Some(controller), Some(tx_power), instance);
        if let Some(channel) = config.channel() {
            handle.set_channel(channel)?;
            handle.pinned.set(Some(channel));
        }
        Ok(handle)
    }

    fn assemble(
        esp_now: EspNow,
        controller: Option<WifiController<'static>>,
        tx_power_quarter_dbm: Option<i8>,
        instance: Singleton<'static>,
    ) -> Self {
        let (manager, sender, receiver) = esp_now.split();
        let owns_controller = controller.is_some();
        Self {
            manager,
            sender: TakeCell::new(sender),
            receiver: TakeCell::new(receiver),
            controller: match controller {
                Some(c) => TakeCell::new(c),
                None => TakeCell::empty(),
            },
            owns_controller,
            tx_power_quarter_dbm: SharedCopy::new(tx_power_quarter_dbm),
            tx: Mutex::new(RefCell::new(TxState {
                core: TxCore::new(),
                full_waiters: WaiterList::new(),
                idle_waiters: WaiterList::new(),
            })),
            runner_wake: Signal::new(),
            runner_active: SharedCopy::new(false),
            receive_gate: Gate::new(),
            oversize: OversizeLog::new(),
            pinned: SharedCopy::new(None),
            _instance: instance,
        }
    }

    /// The station MAC of this chip, the address peers use to reach it.
    ///
    /// An efuse read: valid before the radio starts.
    pub fn sta_mac() -> MacAddress {
        let mac =
            esp_hal::efuse::interface_mac_address(esp_hal::efuse::InterfaceMacAddress::Station);
        let mut out = [0u8; 6];
        out.copy_from_slice(mac.as_bytes());
        out
    }

    /// Sets the primary Wi-Fi channel (`1..=14`).
    ///
    /// For an unassociated station only; an associated station cannot leave its access point's
    /// channel.
    /// This does not change [`EspHalEspNow::pinned_channel`]: that is the channel configured at
    /// start or found by the last scan.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::InvalidChannel`] outside `1..=14`.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the channel.
    pub fn set_channel(&self, channel: u8) -> Result<(), EspHalEspNowError> {
        validate_channel(channel)?;
        self.manager.set_channel(channel).map_err(map_error)
    }

    /// Sets the maximum TX power in quarter dBm (`[8, 84]`, 52 = 13 dBm).
    ///
    /// Works on a handle from [`EspHalEspNow::new_owned`]; a handle from
    /// [`EspHalEspNow::from_controller`] returns [`EspHalEspNowError::ControllerNotOwned`].
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::InvalidTxPower`] for a value outside `[8, 84]`.
    /// * [`EspHalEspNowError::ControllerNotOwned`] on a [`EspHalEspNow::from_controller`] handle.
    /// * [`EspHalEspNowError::Busy`] while another call holds the controller.
    /// * [`EspHalEspNowError::Wifi`] if the driver rejects the value.
    pub fn set_max_tx_power(&self, quarter_dbm: i8) -> Result<(), EspHalEspNowError> {
        validate_tx_power(quarter_dbm)?;
        if !self.owns_controller {
            return Err(EspHalEspNowError::ControllerNotOwned);
        }
        let mut controller = self.controller.lease().ok_or(EspHalEspNowError::Busy)?;
        controller
            .set_max_tx_power(quarter_dbm)
            .map_err(EspHalEspNowError::Wifi)?;
        self.tx_power_quarter_dbm.set(Some(quarter_dbm));
        Ok(())
    }

    /// The TX power last applied by this handle, in quarter dBm.
    ///
    /// `None` on a [`EspHalEspNow::from_controller`] handle whose config set no TX power.
    pub fn tx_power_quarter_dbm(&self) -> Option<i8> {
        self.tx_power_quarter_dbm.get()
    }

    /// Whether `mac` is in the peer table (the broadcast address always is).
    pub fn peer_exists(&self, mac: &MacAddress) -> bool {
        self.manager.peer_exists(mac)
    }

    /// Number of received frames dropped for exceeding [`MAX_DATA_LEN`] since start.
    pub fn oversize_dropped(&self) -> u32 {
        self.oversize.dropped()
    }

    /// The channel the driver keeps pinned, if any.
    ///
    /// That is the channel of [`EspNowConfig::with_channel`] on an owned radio, replaced by the
    /// channel of every successful [`EspHalEspNow::scan_for_peer`].
    /// A failed or cancelled scan leaves it as it was.
    /// `None` on a [`EspHalEspNow::from_controller`] handle before its first scan.
    pub fn pinned_channel(&self) -> Option<u8> {
        self.pinned.get()
    }

    /// Sends and reports the MAC-layer ACK.
    ///
    /// Blocks for the TX duration like [`EspNowDriver::send`], then returns
    /// [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// Broadcast frames always succeed.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspHalEspNowError::ScanInProgress`] while a [`EspHalEspNow::scan_for_peer`] runs.
    /// * [`EspHalEspNowError::Busy`] while another send is in flight (including the sender runner
    ///   mid-transmission).
    /// * [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame (unknown peer, ...).
    pub fn send_and_wait(&self, mac: &MacAddress, data: &[u8]) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
        let mut sender = self.lease_sender()?;
        self.refuse_during_scan()?;
        let waiter = sender.send(mac, data).map_err(map_error)?;
        waiter.wait().map_err(map_error)
    }

    /// Runs the sender: owns every asynchronous transmission of this handle.
    ///
    /// [`EspHalEspNow::send_acked`] and the probes of [`EspHalEspNow::scan_for_peer`] queue their
    /// frame in the shared [`TxCore`] and wait for the result; this runner takes them one at a
    /// time.
    /// `esp-radio` has one global send-callback flag, no sequence number and no way to cancel a
    /// send future, so a transmission is polled to completion here and never dropped; a cancelled
    /// caller only stops waiting.
    /// A request whose deadline passed before the transmission started is not sent
    /// ([`EspHalEspNowError::Timeout`]); a transmission that has started always runs to its end,
    /// and a caller whose deadline passes meanwhile waits for its real result.
    ///
    /// Spawn it exactly once, in an `embassy_executor::task` that is never cancelled:
    ///
    /// ```ignore
    /// #[embassy_executor::task]
    /// async fn espnow_sender(espnow: &'static EspHalEspNow) -> ! {
    ///     espnow.run_sender().await
    /// }
    /// ```
    ///
    /// The runner marks itself active on its first poll, which can come after a task spawned
    /// right behind it already sends.
    /// An async sender therefore waits up to 100 ms (or its own deadline, whichever is
    /// shorter) for the runner to start, and only then returns
    /// [`EspHalEspNowError::NoSenderRunner`]; without a runner at all it fails after that grace.
    /// The sync [`EspNowDriver::send`] and [`EspHalEspNow::send_and_wait`] do not need it.
    /// If the radio never fires the send callback, the runner waits forever (and with it every
    /// async sender), like `esp-radio`'s own send waiter.
    /// Dropping this future by mistake clears the running flag; frames already queued then never
    /// complete.
    ///
    /// The runner asks the queue for work, transmits, and reports back, one frame at a time, so
    /// the queue's "wait until" answer (a frame in flight while queued requests carry deadlines)
    /// cannot arise here; if it ever does, the runner sleeps until that time or the next
    /// submission.
    /// Requests are expired when the runner picks them up, and their callers also time out by
    /// themselves.
    /// A frame is marked in flight when it is handed out, a moment before the sender is leased;
    /// while a sync send still holds the sender, a caller whose deadline passes therefore waits
    /// for that sync send (a few tens of ms) before its own frame goes out.
    ///
    /// Only one runner may run per handle: a second concurrent call logs an error and then does
    /// nothing, forever. It consumes no request and signals no result.
    pub async fn run_sender(&self) -> ! {
        let Some(_active) = Singleton::acquire(&self.runner_active) else {
            log::error!("ESP-NOW: a sender runner is already running; this one does nothing");
            loop {
                core::future::pending::<()>().await;
            }
        };
        loop {
            match self.take_next() {
                Ok(frame) => {
                    let result = self.transmit(&frame).await;
                    self.finish(frame.id, result);
                }
                Err(Some(until)) => {
                    let _ =
                        embassy_time::with_deadline(from_core_time(until), self.runner_wake.wait())
                            .await;
                }
                Err(None) => self.runner_wake.wait().await,
            }
        }
    }

    /// Runs `f` on the send queue under its lock, then wakes whoever it collected.
    ///
    /// `f` must be short and must not call `with_tx` again.
    fn with_tx<T>(&self, f: impl FnOnce(&mut TxState, &mut Wakes) -> T) -> T {
        let mut wakes = Wakes::new();
        let out = self.tx.lock(|state| f(&mut state.borrow_mut(), &mut wakes));
        wakes.wake_all();
        out
    }

    /// Asks the queue for the next frame, copying it out so the lock is released to transmit.
    ///
    /// `Err` carries what to wait for instead: a submission, or also this time when queued
    /// requests carry deadlines.
    fn take_next(&self) -> Result<Frame, Option<Duration>> {
        let now = now_core();
        self.with_tx(
            |state, wakes| match state.core.next(now, &mut |waker| wakes.push(waker)) {
                Next::Transmit { id, mac, data } => {
                    let mut frame = Frame {
                        id,
                        mac,
                        len: data.len(),
                        data: [0; MAX_DATA_LEN],
                    };
                    frame.data[..data.len()].copy_from_slice(data);
                    Ok(frame)
                }
                Next::WaitUntil(until) => Err(Some(until)),
                Next::Idle => Err(None),
            },
        )
    }

    async fn transmit(&self, frame: &Frame) -> TxResult {
        let mut sender = loop {
            match self.sender.lease() {
                Some(lease) => break lease,
                // A sync send holds the sender; it releases it as soon as its frame went out.
                None => {
                    embassy_time::Timer::after_millis(RUNNER_LEASE_RETRY_MS).await;
                }
            }
        };
        sender
            .send_async(&frame.mac, &frame.data[..frame.len])
            .await
            .map_err(map_error)
    }

    /// Reports the result of the frame on the air and wakes its caller, the callers waiting for a
    /// slot and a scan waiting for the air to clear.
    fn finish(&self, id: TxId, result: TxResult) {
        self.with_tx(|state, wakes| {
            if let Some(waker) = state.core.complete(id, result) {
                wakes.push(waker);
            }
            state.idle_waiters.drain_into(wakes);
            state.full_waiters.drain_into(wakes);
        });
    }

    /// Waits for `run_sender` to be polled for the first time, bounded by `deadline` and
    /// [`RUNNER_START_GRACE_MS`].
    ///
    /// The runner sets `runner_active` only on its first poll, and the executor does not
    /// guarantee that a freshly spawned runner runs before another task that sends.
    /// A timer (not `yield_now`) paces the poll: the embassy run queue is a stack, so a
    /// yielding caller is polled again before the runner.
    ///
    /// # Errors
    ///
    /// [`EspHalEspNowError::NoSenderRunner`] when the runner did not appear in time, also when the
    /// caller's own deadline is what ran out: the missing runner is the cause, not a slow radio.
    async fn await_runner(
        &self,
        deadline: Option<embassy_time::Instant>,
    ) -> Result<(), EspHalEspNowError> {
        if self.runner_active.get() {
            return Ok(());
        }
        let grace_end = embassy_time::Instant::now()
            .checked_add(embassy_time::Duration::from_millis(RUNNER_START_GRACE_MS))
            .unwrap_or(embassy_time::Instant::MAX);
        let give_up = runner_wait_deadline(deadline, grace_end);
        while !self.runner_active.get() {
            if expired(Some(give_up), embassy_time::Instant::now()) {
                return Err(EspHalEspNowError::NoSenderRunner);
            }
            embassy_time::Timer::after_millis(RUNNER_START_POLL_MS).await;
        }
        Ok(())
    }

    /// Queues one frame for the runner and waits for its result.
    ///
    /// With a `deadline`, waiting for a free slot, queueing and the start of the transmission are
    /// bounded by it; once the queue has handed the frame to the runner, the transmission is
    /// waited out and its real result returned (`TxCore::poll_expired` decides under the lock).
    ///
    /// Cancel-safe: dropping the future at any point abandons the request (`PendingSend`), so a
    /// queued frame is never transmitted and a frame on the air has its result discarded.
    async fn submit(
        &self,
        mac: &MacAddress,
        data: &[u8],
        deadline: Option<embassy_time::Instant>,
        kind: SendKind,
    ) -> TxResult {
        validate_frame_len(data)?;
        self.await_runner(deadline).await?;
        let id = self.enqueue(mac, data, deadline, kind).await?;
        // No await sits between the queue accepting the request and this guard.
        let mut pending = PendingSend {
            radio: self,
            id: Some(id),
        };
        let result = self.wait_result(id, deadline).await;
        pending.id = None;
        result
    }

    /// Puts the frame in the queue, waiting for a free slot until `deadline`.
    async fn enqueue(
        &self,
        mac: &MacAddress,
        data: &[u8],
        deadline: Option<embassy_time::Instant>,
        kind: SendKind,
    ) -> Result<TxId, EspHalEspNowError> {
        let core_deadline = deadline.map(to_core_time);
        let slot = poll_fn(|cx| {
            self.with_tx(|state, _| {
                match state
                    .core
                    .submit(now_core(), *mac, data, core_deadline, kind)
                {
                    Ok(id) => Poll::Ready(Ok(id)),
                    Err(Refusal::Full) => {
                        if !state.full_waiters.register(cx.waker()) {
                            cx.waker().wake_by_ref();
                        }
                        Poll::Pending
                    }
                    Err(refusal) => Poll::Ready(Err(refusal_error(refusal, data.len()))),
                }
            })
        });
        let id = bounded(deadline, slot)
            .await
            .ok_or(EspHalEspNowError::Timeout)??;
        self.runner_wake.signal(());
        Ok(id)
    }

    /// Waits for the result of request `id`, honouring `deadline` for queued requests only.
    async fn wait_result(&self, id: TxId, deadline: Option<embassy_time::Instant>) -> TxResult {
        let completion = match bounded(deadline, self.poll_completion(id, false)).await {
            Some(completion) => completion,
            // Deadline passed: a queued request expires here, a frame on the air is waited out.
            None => self.poll_completion(id, true).await,
        };
        completion_result(completion)
    }

    async fn poll_completion(&self, id: TxId, past_deadline: bool) -> Completion<TxResult> {
        poll_fn(|cx| {
            self.with_tx(|state, wakes| {
                let poll = if past_deadline {
                    state.core.poll_expired(id, cx.waker())
                } else {
                    state.core.poll_result(id, cx.waker())
                };
                if poll.is_ready() {
                    state.full_waiters.drain_into(wakes);
                }
                poll
            })
        })
        .await
    }

    /// Refuses a sync send while a scan holds the radio.
    ///
    /// Call it only while holding the sender lease: a scan waits for the sender to be free before
    /// its first radio action, so a send that leased first finishes before the scan touches the
    /// radio, and one that leased after `begin_scan` sees the scan here.
    fn refuse_during_scan(&self) -> Result<(), EspHalEspNowError> {
        if self.with_tx(|state, _| state.core.is_scanning()) {
            Err(EspHalEspNowError::ScanInProgress)
        } else {
            Ok(())
        }
    }

    /// Sends and reports the MAC-layer ACK, without blocking the executor.
    ///
    /// `Ok` means the unicast peer acknowledged the frame; [`EspHalEspNowError::SendFailed`] means
    /// it did not.
    /// The transmission runs in [`EspHalEspNow::run_sender`]; concurrent callers queue up behind
    /// each other.
    /// `timeout` bounds waiting for a queue slot, queueing and the start of the transmission; a
    /// transmission that has already started is waited out and its real result returned.
    /// A request that did not start in time is never sent: the runner drops it.
    /// [`Duration::MAX`] means no bound.
    /// Dropping the future is safe: a queued frame is never sent, and one on the air has its
    /// result discarded.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspHalEspNowError::Broadcast`] for the broadcast address: broadcast frames are never
    ///   acknowledged, use [`EspNowDriver::send`].
    /// * [`EspHalEspNowError::ScanInProgress`] while a [`EspHalEspNow::scan_for_peer`] runs, or
    ///   when one began before this frame started.
    /// * [`EspHalEspNowError::NoSenderRunner`] when [`EspHalEspNow::run_sender`] did not start
    ///   within 100 ms (or the timeout, if shorter).
    /// * [`EspHalEspNowError::Timeout`] if the frame did not start within `timeout`.
    /// * [`EspHalEspNowError::SendFailed`] when the frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame.
    pub async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), EspHalEspNowError> {
        let deadline = embassy_time::Instant::now().checked_add(to_embassy(timeout));
        self.submit(mac, data, deadline, SendKind::App).await
    }

    /// One scan probe: a [`SendKind::Probe`] frame with `timeout` as one absolute deadline for
    /// waiting on a slot, the transmission and its ACK.
    /// Nothing is queued when no time is left ([`EspHalEspNowError::Timeout`]).
    pub(crate) async fn send_probe(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
        if timeout.is_zero() {
            return Err(EspHalEspNowError::Timeout);
        }
        let deadline = embassy_time::Instant::now().checked_add(to_embassy(timeout));
        self.submit(mac, data, deadline, SendKind::Probe).await
    }

    /// Waits for the next valid frame (at most [`MAX_DATA_LEN`] bytes); oversize frames are
    /// dropped and counted.
    ///
    /// Safe to call from several tasks at once: they take turns, and each frame goes to exactly
    /// one of them.
    /// No order among the waiting callers is promised.
    /// While a call is pending, [`EspNowDriver::try_recv`] returns `None`.
    pub async fn receive(&self) -> EspNowEvent {
        let _turn = self.receive_gate.acquire().await;
        // Only this method takes the receiver out, and the gate lets one call in at a time, so
        // the lease is always free; the retry only keeps this path panic-free.
        let mut receiver = loop {
            match self.receiver.lease() {
                Some(lease) => break lease,
                None => {
                    embassy_time::Timer::after_millis(RUNNER_LEASE_RETRY_MS).await;
                }
            }
        };
        loop {
            let frame = receiver.receive_async().await;
            if let Some(event) = self.event_from(frame) {
                return event;
            }
        }
    }

    /// Finds the Wi-Fi channel on which the peer `mac` answers, and leaves the radio and the peer
    /// table set up for it.
    ///
    /// Drives the shared [`ScanMachine`], so the semantics are those of the ESP-IDF tier's
    /// scan: stale-peer removal, a channel-0 temporary peer, one MAC-ACK probe per
    /// channel, `probe_confirmations` further probes `confirmation_gap` apart, a `burst_timeout`
    /// budget, and on failure the radio and the peer registration go back to the pinned channel.
    ///
    /// The driver keeps the pin ([`EspHalEspNow::pinned_channel`]): the channel of the previous
    /// successful scan, or the one set with [`EspNowConfig::with_channel`], is where a failed
    /// scan puts the radio back.
    /// It is updated on success and left alone on failure and cancellation.
    ///
    /// Needs a handle from [`EspHalEspNow::new_owned`], which owns the radio: moving an
    /// associated station across channels would drop its access-point link.
    /// Probes go through [`EspHalEspNow::run_sender`]; a probe waits up to 100 ms (or its
    /// timeout) for a just-spawned runner to start.
    /// Without a runner every probe fails, so the scan reports
    /// [`ScanError::NotFound`](juggler::espnow::ScanError::NotFound) with a probe-send
    /// `first_fault` and `probed: 0`, and logs the cause at `warn`.
    ///
    /// The scan holds the radio against application sends from the moment it is admitted until
    /// channel, TX power and peer registration are restored.
    /// Application sends during that time fail with [`EspHalEspNowError::ScanInProgress`], and
    /// those still queued when the scan begins are dropped unsent.
    /// A frame already on the air is waited out before the first radio action.
    ///
    /// The TX power is boosted to [`TxPowerLevel::Max`] for the burst and restored afterwards,
    /// including when the future is dropped mid-scan.
    ///
    /// Cancelling the future (dropping it mid-scan) restores the channel, the peer registration and
    /// the TX power synchronously in `Drop` (ADR 020 exception, `Drop` cannot await): the
    /// temporary peer is removed and, when a channel was pinned, the radio and the peer go back to
    /// it; only then does the exclusion end.
    ///
    /// Each probe is bounded by `config.probe_timeout`; a timeout means the frame never started
    /// and counts as a local failure (not as "no ACK"); a transmitted but unacknowledged frame is "no ACK".
    /// A cancelled or timed-out probe never orphans a completion.
    /// The scan never changes channel while a probe is being transmitted: a probe that has started
    /// is waited out (the burst budget counts the overrun, the machine is fed the real time), and
    /// one that has not started is dropped by the runner.
    /// No lock is held across an await point.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::ControllerNotOwned`] on a [`EspHalEspNow::from_controller`] handle.
    /// * [`EspHalEspNowError::Busy`] while another `scan_for_peer` call is running; scans are
    ///   serialized.
    /// * [`EspHalEspNowError::Scan`] with the [`ScanError`](juggler::espnow::ScanError) reported
    ///   by the machine.
    pub async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, EspHalEspNowError> {
        self.with_tx(|state, wakes| {
            state
                .core
                .begin_scan(self.owns_controller, &mut |waker| wakes.push(waker))
        })
        .map_err(scan_refusal_error)?;
        // Declared first, so it drops last: the exclusion ends only after the restore below.
        let _exclusion = ScanExclusion { radio: self };
        self.wait_scan_idle().await;
        let pinned = self.pinned.get();
        let mut machine = ScanMachine::new(*mac, *config, pinned);
        let start = embassy_time::Instant::now();
        // Declared before `cleanup`, so on cancellation the channel and peer are restored first and
        // the TX power afterwards.
        let mut boost = TxBoost::new(self);
        let mut cleanup = ScanCleanup::new(self, *mac, pinned);
        let mut input = ScanInput::Start;
        let result = loop {
            let now = Duration::from_micros(start.elapsed().as_micros());
            input = match machine.poll(now, input) {
                ScanAction::RemovePeer => completed(self.remove_peer(mac)),
                ScanAction::RegisterPeer { channel }
                | ScanAction::RegisterFallbackPeer { channel } => {
                    cleanup.arm();
                    let mut peer = PeerConfig::new(*mac);
                    peer.channel = channel;
                    completed(self.add_peer(&peer))
                }
                ScanAction::SetChannel { channel } => {
                    cleanup.arm();
                    let result = self.set_channel(channel);
                    if let Err(e) = &result {
                        machine.log_fault(format_args!("failed to set channel {channel}: {e:?}"));
                    }
                    completed(result)
                }
                ScanAction::RestoreChannel { channel } => {
                    cleanup.arm();
                    completed(self.set_channel(channel))
                }
                ScanAction::BoostTxPower => {
                    // Advisory: without a controller the scan runs at the caller's level.
                    boost.engage();
                    ScanInput::Completed
                }
                ScanAction::RestoreTxPower => {
                    boost.release();
                    ScanInput::Completed
                }
                ScanAction::SendProbe {
                    channel, timeout, ..
                } => {
                    let result = self.send_probe(mac, machine.probe_data(), timeout).await;
                    let outcome = SendOutcome::of(&result);
                    if let (SendOutcome::Local, Err(e)) = (outcome, &result) {
                        machine.log_fault(format_args!("probe on channel {channel} failed: {e:?}"));
                    }
                    ScanInput::Sent(outcome)
                }
                ScanAction::WaitUntil(deadline) => {
                    embassy_time::Timer::at(start + to_embassy(deadline)).await;
                    ScanInput::Completed
                }
                ScanAction::Done(result) => {
                    cleanup.disarm();
                    break result;
                }
            };
        };
        // Covers a machine that finished without reaching `RestoreTxPower`.
        boost.release();
        self.pinned.set(machine.pinned_channel());
        result.map_err(EspHalEspNowError::Scan)
    }

    /// Waits until no frame is on the air and no sync send holds the sender, so the scan's first
    /// radio action cannot change the channel under a transmission.
    async fn wait_scan_idle(&self) {
        poll_fn(|cx| {
            self.with_tx(|state, _| {
                if state.core.can_start_scan_io() {
                    Poll::Ready(())
                } else {
                    if !state.idle_waiters.register(cx.waker()) {
                        cx.waker().wake_by_ref();
                    }
                    Poll::Pending
                }
            })
        })
        .await;
        while self.sender.with_locked(|_| ()).is_none() {
            embassy_time::Timer::after_millis(SCAN_SENDER_POLL_MS).await;
        }
    }

    /// Leases the sender for a blocking send.
    fn lease_sender(&self) -> Result<Lease<'_, EspNowSender>, EspHalEspNowError> {
        self.sender.lease().ok_or(EspHalEspNowError::Busy)
    }

    /// Converts a received frame; oversize frames are counted and dropped, never truncated.
    fn event_from(&self, frame: ReceivedData) -> Option<EspNowEvent> {
        match EspNowEvent::try_new(frame.info.src_address, frame.data()) {
            Ok(event) => Some(event),
            Err(len) => {
                if self.oversize.record() {
                    log::warn!(
                        "ESP-NOW: dropped oversize frame ({} B > {} B), {} dropped in total",
                        len,
                        MAX_DATA_LEN,
                        self.oversize.dropped()
                    );
                }
                None
            }
        }
    }
}

impl EspNowDriver for EspHalEspNow {
    type Error = EspHalEspNowError;

    fn add_peer(&self, config: &PeerConfig) -> Result<(), Self::Error> {
        let plan = plan_peer(config)?;
        let info = peer_info(&plan);
        match self.manager.add_peer(info) {
            Ok(()) => Ok(()),
            Err(EspNowError::Error(esp_radio::esp_now::Error::PeerExists))
                if existing_peer_is_idempotent(&plan) =>
            {
                // `esp-radio` pre-registers the broadcast peer on the station interface.
                if plan.channel.is_some() {
                    self.manager.modify_peer(info).map_err(map_error)
                } else {
                    Ok(())
                }
            }
            Err(e) => Err(map_error(e)),
        }
    }

    fn remove_peer(&self, mac: &MacAddress) -> Result<(), Self::Error> {
        self.manager.remove_peer(mac).map_err(map_error)
    }

    /// Blocks until the send callback fired; the MAC-layer ACK is ignored (see
    /// [`EspHalEspNow::send_and_wait`]).
    fn send(&self, mac: &MacAddress, data: &[u8]) -> Result<(), Self::Error> {
        validate_frame_len(data)?;
        let mut sender = self.lease_sender()?;
        self.refuse_during_scan()?;
        let waiter = sender.send(mac, data).map_err(map_error)?;
        // The ACK outcome is deliberately ignored, exactly like the IDF `send`.
        let _ = waiter.wait();
        Ok(())
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        for _ in 0..RX_SKIP_LIMIT {
            // Pop under the lock, convert (and free the frame) outside it.
            let frame = self.receiver.with_locked(|r| r.receive()).flatten()?;
            if let Some(event) = self.event_from(frame) {
                return Some(event);
            }
        }
        None
    }
}

impl EspNowLink for EspHalEspNow {
    async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        EspHalEspNow::send_acked(self, mac, data, timeout).await
    }

    async fn receive(&self) -> EspNowEvent {
        EspHalEspNow::receive(self).await
    }

    async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, Self::Error> {
        EspHalEspNow::scan_for_peer(self, mac, config).await
    }

    fn pinned_channel(&self) -> Option<u8> {
        EspHalEspNow::pinned_channel(self)
    }
}

/// Tier parity check (ADR 022): fails to compile if `EspHalEspNow` stops implementing `EspNowLink`.
const _: fn() = || {
    fn implements<T: EspNowLink>()
    where
        <T as EspNowDriver>::Error: juggler::espnow::ClassifySend,
    {
    }
    implements::<EspHalEspNow>();
};

/// Abandons a request when its caller is dropped before the result was taken.
struct PendingSend<'a> {
    radio: &'a EspHalEspNow,
    /// Cleared once the result has been consumed: a consumed id may be reused.
    id: Option<TxId>,
}

impl Drop for PendingSend<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.radio.with_tx(|state, wakes| {
                state.core.abandon(id);
                state.full_waiters.drain_into(wakes);
            });
        }
    }
}

/// Holds the scan exclusion of [`TxCore::begin_scan`]; ends it on drop.
///
/// Declared before the scan's restore guards, so it drops after them: the exclusion ends only once
/// channel, TX power and pin are restored.
struct ScanExclusion<'a> {
    radio: &'a EspHalEspNow,
}

impl Drop for ScanExclusion<'_> {
    fn drop(&mut self) {
        self.radio.with_tx(|state, _| state.core.end_scan());
    }
}

/// Now on the [`TxCore`] clock: time since the embassy epoch.
fn now_core() -> Duration {
    to_core_time(embassy_time::Instant::now())
}

fn to_core_time(at: embassy_time::Instant) -> Duration {
    Duration::from_micros(at.as_micros())
}

fn from_core_time(at: Duration) -> embassy_time::Instant {
    embassy_time::Instant::from_micros(u64::try_from(at.as_micros()).unwrap_or(u64::MAX))
}

fn completed(result: Result<(), EspHalEspNowError>) -> ScanInput {
    match result {
        Ok(()) => ScanInput::Completed,
        Err(_) => ScanInput::Failed,
    }
}

/// Scan-burst TX power: raises the power on [`TxBoost::engage`] and puts the previous level back
/// on [`TxBoost::release`] or, as a safety net, when dropped (cancelled scan future).
/// The level restored is the one saved at [`TxBoost::engage`], not whatever was set since.
struct TxBoost<'a> {
    radio: &'a EspHalEspNow,
    saved: Option<i8>,
}

/// Puts the radio back when a scan future is dropped mid-scan: removes the temporary peer and,
/// when a channel was pinned before the scan, returns the radio and the peer to it.
/// This is the machine's own failure fallback, run synchronously.
struct ScanCleanup<'a> {
    radio: &'a EspHalEspNow,
    mac: MacAddress,
    pinned: Option<u8>,
    armed: bool,
}

impl<'a> ScanCleanup<'a> {
    fn new(radio: &'a EspHalEspNow, mac: MacAddress, pinned: Option<u8>) -> Self {
        Self {
            radio,
            mac,
            pinned,
            armed: false,
        }
    }

    /// Called once the scan has touched the channel or the peer table.
    fn arm(&mut self) {
        self.armed = true;
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ScanCleanup<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // The peer may not be registered yet; that is not worth a warning.
        let _ = self.radio.remove_peer(&self.mac);
        if let Some(channel) = self.pinned {
            let mut peer = PeerConfig::new(self.mac);
            peer.channel = channel;
            if let Err(e) = self
                .radio
                .set_channel(channel)
                .and_then(|()| self.radio.add_peer(&peer))
            {
                log::warn!("ESP-NOW scan cancelled: could not restore channel {channel}: {e:?}");
            }
        }
    }
}

/// Awaits `fut` until `deadline`; `None` when the deadline passed first. Without a deadline, waits.
async fn bounded<F: core::future::Future>(
    deadline: Option<embassy_time::Instant>,
    fut: F,
) -> Option<F::Output> {
    match deadline {
        Some(at) => embassy_time::with_deadline(at, fut).await.ok(),
        None => Some(fut.await),
    }
}

fn to_embassy(timeout: Duration) -> embassy_time::Duration {
    embassy_time::Duration::try_from(timeout).unwrap_or(embassy_time::Duration::MAX)
}

impl<'a> TxBoost<'a> {
    fn new(radio: &'a EspHalEspNow) -> Self {
        Self { radio, saved: None }
    }

    fn engage(&mut self) {
        if self.saved.is_some() {
            return;
        }
        let Some(previous) = self.radio.tx_power_quarter_dbm() else {
            return;
        };
        let max = TxPowerLevel::Max.to_quarter_dbm();
        if self.radio.set_max_tx_power(max).is_ok() {
            self.saved = Some(previous);
        }
    }

    fn release(&mut self) {
        if let Some(previous) = self.saved.take() {
            if self.radio.set_max_tx_power(previous).is_err() {
                log::warn!("ESP-NOW scan: could not restore TX power to {previous} quarter dBm");
            }
        }
    }
}

impl Drop for TxBoost<'_> {
    fn drop(&mut self) {
        self.release();
    }
}

fn peer_info(plan: &PeerPlan) -> PeerInfo {
    PeerInfo {
        interface: match plan.interface {
            WifiInterface::Sta => EspNowWifiInterface::Station,
            WifiInterface::Ap => EspNowWifiInterface::AccessPoint,
        },
        peer_address: plan.mac,
        lmk: None,
        channel: plan.channel,
        encrypt: false,
    }
}

fn map_error(error: EspNowError) -> EspHalEspNowError {
    use esp_radio::esp_now::Error as Raw;
    match error {
        EspNowError::SendFailed => EspHalEspNowError::SendFailed,
        EspNowError::DuplicateInstance => EspHalEspNowError::AlreadyInitialised,
        EspNowError::Initialization(w) => EspHalEspNowError::Wifi(w),
        EspNowError::Error(raw) => EspHalEspNowError::Radio(match raw {
            Raw::NotInitialized => RadioError::NotInitialized,
            Raw::InvalidArgument => RadioError::InvalidArgument,
            Raw::OutOfMemory => RadioError::OutOfMemory,
            Raw::PeerListFull => RadioError::PeerListFull,
            Raw::NotFound => RadioError::NotFound,
            Raw::Internal => RadioError::Internal,
            Raw::PeerExists => RadioError::PeerExists,
            Raw::InterfaceMismatch => RadioError::InterfaceMismatch,
            Raw::Other(code) => RadioError::Other(code),
        }),
    }
}
