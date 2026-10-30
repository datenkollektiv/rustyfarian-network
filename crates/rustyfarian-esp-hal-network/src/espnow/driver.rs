//! Hardware half of the ESP-NOW driver: `esp-radio` glue behind [`EspHalEspNow`].

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use esp_radio::esp_now::{
    EspNow, EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface,
    PeerInfo, ReceivedData,
};
use esp_radio::wifi::{ControllerConfig, WifiController};
use juggler::espnow::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, ScanAction, ScanConfig, ScanInput,
    ScanMachine, ScanResult, WifiInterface,
};
use juggler::wifi::TxPowerLevel;

use super::cell::{Lease, SharedCopy, Singleton, TakeCell};
use super::{
    existing_peer_is_idempotent, expired, frame_event, plan_peer, probe_outcome,
    should_log_oversize, validate_channel, validate_frame_len, validate_tx_power,
    EspHalEspNowError, EspNowConfig, PeerPlan, RadioError, DEFAULT_TX_POWER_QUARTER_DBM,
    MAX_DATA_LEN,
};

/// Frames examined per [`EspNowDriver::try_recv`] call when the head of the queue is oversize.
const RX_SKIP_LIMIT: usize = 16;

/// Pause between attempts of the sender runner to lease a sender that a sync send holds.
const RUNNER_LEASE_RETRY_MS: u64 = 1;

/// One transmission handed from a caller to [`EspHalEspNow::run_sender`]; the payload is copied.
struct SendRequest {
    id: u32,
    mac: MacAddress,
    len: u8,
    data: [u8; MAX_DATA_LEN],
    /// The runner completes the request with `Timeout` instead of sending it once this has passed.
    deadline: Option<embassy_time::Instant>,
}

/// Outcome of the request with the same `id`.
#[derive(Clone, Copy)]
struct SendDone {
    id: u32,
    result: Result<(), EspHalEspNowError>,
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
    /// Transmissions queued for the sender runner.
    requests: Channel<CriticalSectionRawMutex, SendRequest, 1>,
    /// Result of the transmission the runner finished last.
    done: Signal<CriticalSectionRawMutex, SendDone>,
    /// Serializes async callers from enqueue until their result is consumed.
    callers: Mutex<CriticalSectionRawMutex, ()>,
    next_id: SharedCopy<u32>,
    /// Set while [`EspHalEspNow::run_sender`] is being polled.
    runner_active: SharedCopy<bool>,
    /// Id of the request the runner has taken: set before its expiry check, cleared after its
    /// `SendDone` was signalled.
    in_flight: SharedCopy<Option<u32>>,
    oversize_dropped: SharedCopy<u32>,
    /// Set while a [`EspHalEspNow::scan_for_peer`] call runs.
    scan_active: SharedCopy<bool>,
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
            requests: Channel::new(),
            done: Signal::new(),
            callers: Mutex::new(()),
            next_id: SharedCopy::new(0),
            runner_active: SharedCopy::new(false),
            in_flight: SharedCopy::new(None),
            oversize_dropped: SharedCopy::new(0),
            scan_active: SharedCopy::new(false),
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

    /// Pins the primary Wi-Fi channel (`1..=14`).
    ///
    /// For an unassociated station only; an associated station cannot leave its access point's
    /// channel.
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
        self.oversize_dropped.get()
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
    /// * [`EspHalEspNowError::Busy`] while another send is in flight (including the sender runner
    ///   mid-transmission).
    /// * [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame (unknown peer, ...).
    pub fn send_and_wait(&self, mac: &MacAddress, data: &[u8]) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
        let mut sender = self.lease_sender()?;
        let waiter = sender.send(mac, data).map_err(map_error)?;
        waiter.wait().map_err(map_error)
    }

    /// Runs the sender: owns every asynchronous transmission of this handle.
    ///
    /// [`EspHalEspNow::send_async`], [`EspHalEspNow::send_async_timeout`] and the probes of
    /// [`EspHalEspNow::scan_for_peer`] queue their frame here and wait for the result.
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
    /// Without a running runner the async senders return [`EspHalEspNowError::NoSenderRunner`]
    /// at once; the sync [`EspNowDriver::send`] and [`EspHalEspNow::send_and_wait`] do not need it.
    /// If the radio never fires the send callback, the runner waits forever (and with it every
    /// async sender), like `esp-radio`'s own send waiter.
    /// Dropping this future by mistake clears the running flag; frames already queued then never
    /// complete.
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
        self.in_flight.set(None);
        loop {
            let request = self.requests.receive().await;
            let result = self.transmit(&request).await;
            self.done.signal(SendDone {
                id: request.id,
                result,
            });
            self.in_flight.set(None);
        }
    }

    async fn transmit(&self, request: &SendRequest) -> Result<(), EspHalEspNowError> {
        let mut sender = loop {
            match self.sender.lease() {
                Some(lease) => break lease,
                // A sync send holds the sender; it releases it as soon as its frame went out.
                None => {
                    embassy_time::Timer::after_millis(RUNNER_LEASE_RETRY_MS).await;
                }
            }
        };
        // Marked only once the sender is held, so a caller whose deadline passes while a sync
        // send still holds the lease is not made to wait for a transmission that never started.
        // Set before the expiry check: see the invariant on `submit`.
        self.in_flight.set(Some(request.id));
        if expired(request.deadline, embassy_time::Instant::now()) {
            return Err(EspHalEspNowError::Timeout);
        }
        let data = &request.data[..usize::from(request.len)];
        sender
            .send_async(&request.mac, data)
            .await
            .map_err(map_error)
    }

    /// Queues one frame for the runner and waits for its result.
    ///
    /// With a `deadline`, queueing, waiting for the sender and the start of the transmission are
    /// bounded by it; once the runner holds the sender for the request (`in_flight`), the
    /// transmission is waited out and its real result returned.
    ///
    /// Invariant behind the timeout path: the runner sets `in_flight` to the request's id only when
    /// it holds the sender, before it reads the clock for the expiry check, and the caller reads
    /// `in_flight` after its deadline passed. Either the caller sees the id (the runner may be
    /// transmitting: wait for the result), or the runner reaches the expiry check later than the
    /// caller's read, hence after the deadline, and drops the request as expired. The runner never
    /// starts a transmission that the caller has abandoned, and a request stuck behind a sync send
    /// past its deadline is dropped, not transmitted.
    ///
    /// Cancel-safe: dropping the future at any point leaves at most a queued request that the
    /// runner still completes (or drops as expired); its result is skipped by the next caller (ids
    /// differ).
    async fn submit(
        &self,
        mac: &MacAddress,
        data: &[u8],
        deadline: Option<embassy_time::Instant>,
    ) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
        if !self.runner_active.get() {
            return Err(EspHalEspNowError::NoSenderRunner);
        }
        // Held until the result is consumed, so no `SendDone` is overwritten before its caller
        // has read it.
        let _caller = bounded(deadline, self.callers.lock())
            .await
            .ok_or(EspHalEspNowError::Timeout)?;
        let id = self.next_id.update(|n| n.wrapping_add(1));
        let mut request = SendRequest {
            id,
            mac: *mac,
            // Checked against `MAX_DATA_LEN` (250) above.
            len: data.len() as u8,
            data: [0; MAX_DATA_LEN],
            deadline,
        };
        request.data[..data.len()].copy_from_slice(data);
        bounded(deadline, self.requests.send(request))
            .await
            .ok_or(EspHalEspNowError::Timeout)?;
        if let Some(result) = bounded(deadline, self.wait_done(id)).await {
            return result;
        }
        // Deadline passed: see the invariant above.
        if self.in_flight.get() == Some(id) && self.runner_active.get() {
            return self.wait_done(id).await;
        }
        match self.done.try_take() {
            Some(done) if done.id == id => done.result,
            _ => Err(EspHalEspNowError::Timeout),
        }
    }

    /// Waits for the `SendDone` of request `id`, skipping results of earlier requests.
    async fn wait_done(&self, id: u32) -> Result<(), EspHalEspNowError> {
        loop {
            let done = self.done.wait().await;
            if done.id == id {
                return done.result;
            }
        }
    }

    /// Sends without blocking the executor and reports the MAC-layer ACK.
    ///
    /// `Ok` means a unicast peer acknowledged the frame; [`EspHalEspNowError::SendFailed`] means
    /// it did not.
    /// The transmission runs in [`EspHalEspNow::run_sender`]; concurrent callers queue up behind
    /// each other.
    /// There is no timeout: use [`EspHalEspNow::send_async_timeout`] to bound the wait.
    /// Dropping the future is safe: the runner still completes the transmission.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspHalEspNowError::NoSenderRunner`] when [`EspHalEspNow::run_sender`] is not running.
    /// * [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame.
    pub async fn send_async(&self, mac: &MacAddress, data: &[u8]) -> Result<(), EspHalEspNowError> {
        self.submit(mac, data, None).await
    }

    /// [`EspHalEspNow::send_async`] with a bounded wait.
    ///
    /// `timeout` bounds queueing and the start of transmission; a transmission that has already
    /// started is waited out and its real result returned.
    /// A request that did not start in time is never sent: the runner drops it.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspHalEspNowError::NoSenderRunner`] when [`EspHalEspNow::run_sender`] is not running.
    /// * [`EspHalEspNowError::Timeout`] if no completion arrived within `timeout`.
    /// * [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame.
    pub async fn send_async_timeout(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: core::time::Duration,
    ) -> Result<(), EspHalEspNowError> {
        let deadline = embassy_time::Instant::now().checked_add(to_embassy(timeout));
        self.submit(mac, data, deadline).await
    }

    /// One scan probe: [`EspHalEspNow::send_async_timeout`] with `timeout` as one absolute
    /// deadline for waiting on other callers, the transmission and its ACK.
    /// Nothing is queued when no time is left ([`EspHalEspNowError::Timeout`]).
    pub(crate) async fn send_probe(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: core::time::Duration,
    ) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
        if timeout.is_zero() {
            return Err(EspHalEspNowError::Timeout);
        }
        self.send_async_timeout(mac, data, timeout).await
    }

    /// Waits for the next valid frame (at most [`MAX_DATA_LEN`] bytes); oversize frames are
    /// dropped and counted.
    ///
    /// One waiter at a time: a second concurrent call returns [`EspHalEspNowError::Busy`].
    /// While a call is pending, [`EspNowDriver::try_recv`] returns `None`.
    ///
    /// # Errors
    ///
    /// [`EspHalEspNowError::Busy`] while another receive call is pending.
    pub async fn recv_async(&self) -> Result<EspNowEvent, EspHalEspNowError> {
        let mut receiver = self.receiver.lease().ok_or(EspHalEspNowError::Busy)?;
        loop {
            let frame = receiver.receive_async().await;
            if let Some(event) = self.event_from(frame) {
                return Ok(event);
            }
        }
    }

    /// Finds the Wi-Fi channel on which the peer `mac` answers, and leaves the radio and the peer
    /// table set up for it.
    ///
    /// Drives the shared [`ScanMachine`], so the semantics are those of the ESP-IDF tier's
    /// `scan_for_peer`: stale-peer removal, a channel-0 temporary peer, one MAC-ACK probe per
    /// channel, `probe_confirmations` further probes `confirmation_gap` apart, a `burst_timeout`
    /// budget, and on failure the radio and the peer registration go back to `pinned`.
    ///
    /// `pinned` holds the channel of the previous successful scan (`None` before the first scan).
    /// It is updated on return, on success and failure alike; keep it and pass it to the next scan.
    ///
    /// Meant for a handle from [`EspHalEspNow::new_owned`], which owns the radio.
    /// On an associated station (a [`EspHalEspNow::from_controller`] handle) the scan moves the
    /// radio to every channel in turn and drops the access-point link.
    /// Probes go through [`EspHalEspNow::run_sender`] (without it the scan fails with
    /// [`EspHalEspNowError::NoSenderRunner`] on the first probe).
    /// A concurrent periodic sender queues in front of a probe and eats into its timeout, so it
    /// can still delay or skew the result.
    ///
    /// The TX power is boosted to [`TxPowerLevel::Max`] for the burst and restored afterwards,
    /// including when the future is dropped mid-scan.
    /// A handle from [`EspHalEspNow::from_controller`] cannot change TX power
    /// ([`EspHalEspNowError::ControllerNotOwned`]); the scan then runs at the caller's level.
    ///
    /// Cancelling the future (dropping it mid-scan) restores the channel, the peer registration and
    /// the TX power: the temporary peer is removed and, when `pinned` held a channel, the radio and
    /// the peer go back to it.
    /// `pinned` is left unchanged in that case, so it still describes the restored state.
    ///
    /// Each probe is bounded by `config.probe_timeout`; a timeout counts as "no ACK".
    /// A cancelled or timed-out probe never orphans a completion (the runner finishes it).
    /// The scan never changes channel while a probe is being transmitted: a probe that has started
    /// is waited out (the burst budget counts the overrun, the machine is fed the real time), and
    /// one that has not started is dropped by the runner.
    /// The exception is cancellation, where `ScanCleanup` restores the channel immediately
    /// (best-effort, `Drop` cannot await).
    /// No lock is held across an await point.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::Busy`] while another `scan_for_peer` call is running; scans are
    ///   serialized.
    /// * [`EspHalEspNowError::Scan`] with the [`ScanError`](juggler::espnow::ScanError) reported
    ///   by the machine.
    pub async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
        pinned: &mut Option<u8>,
    ) -> Result<ScanResult, EspHalEspNowError> {
        let _slot = Singleton::acquire(&self.scan_active).ok_or(EspHalEspNowError::Busy)?;
        let mut machine = ScanMachine::new(*mac, *config, *pinned);
        let start = embassy_time::Instant::now();
        // Declared before `cleanup`, so on cancellation the channel and peer are restored first and
        // the TX power afterwards.
        let mut boost = TxBoost::new(self);
        let mut cleanup = ScanCleanup::new(self, *mac, *pinned);
        let mut input = ScanInput::Start;
        let result = loop {
            let now = core::time::Duration::from_micros(start.elapsed().as_micros());
            input = match machine.poll(now, input) {
                ScanAction::RemovePeer => completed(self.remove_peer(mac)),
                ScanAction::RegisterPeer { channel }
                | ScanAction::RegisterFallbackPeer { channel } => {
                    cleanup.arm();
                    let mut peer = PeerConfig::new(*mac);
                    peer.channel = channel;
                    completed(self.add_peer(&peer))
                }
                ScanAction::SetChannel { channel } | ScanAction::RestoreChannel { channel } => {
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
                ScanAction::SendProbe { timeout, .. } => ScanInput::Sent(probe_outcome(
                    self.send_probe(mac, machine.probe_data(), timeout).await,
                )),
                ScanAction::WaitUntil(deadline) => {
                    let micros = u64::try_from(deadline.as_micros()).unwrap_or(u64::MAX);
                    embassy_time::Timer::at(start + embassy_time::Duration::from_micros(micros))
                        .await;
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
        *pinned = machine.pinned_channel();
        result.map_err(EspHalEspNowError::Scan)
    }

    /// Leases the sender for a blocking send.
    fn lease_sender(&self) -> Result<Lease<'_, EspNowSender>, EspHalEspNowError> {
        self.sender.lease().ok_or(EspHalEspNowError::Busy)
    }

    /// Converts a received frame; oversize frames are counted and dropped, never truncated.
    fn event_from(&self, frame: ReceivedData) -> Option<EspNowEvent> {
        match frame_event(frame.info.src_address, frame.data()) {
            Ok(event) => Some(event),
            Err(len) => {
                let total = self.oversize_dropped.update(|n| n.wrapping_add(1));
                if should_log_oversize(total) {
                    log::warn!(
                        "ESP-NOW: dropped oversize frame ({} B > {} B), {} dropped in total",
                        len,
                        MAX_DATA_LEN,
                        total
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

fn to_embassy(timeout: core::time::Duration) -> embassy_time::Duration {
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
