//! Hardware half of the ESP-NOW driver: `esp-radio` glue behind [`EspHalEspNow`].

use core::cell::RefCell;
use core::future::poll_fn;
use core::task::Poll;
use core::time::Duration;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use esp_radio::esp_now::{
    EspNow, EspNowError, EspNowManager, EspNowReceiver, EspNowSender, EspNowWifiInterface,
    PeerInfo, ReceivedData,
};
use esp_radio::wifi::{ControllerConfig, WifiController};
use juggler::espnow::{
    CancelOutcome, Completed, Completion, EspNowDriver, EspNowEvent, EspNowLink, GateRefusal,
    JobEpoch, MacAddress, Next, OversizeLog, PeerConfig, ProbeReply, Refusal, ScanAction,
    ScanConfig, ScanError, ScanGate, ScanInput, ScanMachine, ScanResult, ScanTeardown, SendKind,
    SendOutcome, TxCore, TxId, WifiInterface,
};
use juggler::wifi::{apply_country, ChannelSet, Country, CountryPort, Regulatory, TxPowerLevel};

use super::cell::{Gate, Lease, SharedCopy, Singleton, TakeCell, WaiterList, Wakeups};
use super::{
    completion_result, existing_peer_is_idempotent, expired, gate_refusal_error, plan_peer,
    refusal_error, runner_wait_deadline, validate_channel, validate_frame_len, validate_tx_power,
    EspHalEspNowError, EspNowConfig, HoldingBuffer, PeerPlan, RadioError, ReplyMailbox,
    MAX_DATA_LEN,
};
use crate::wifi::RadioCountry;

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

/// Request slots of the send queue.
///
/// [`TxCore`] keeps one slot for the scan's probe, so application requests (a waiting caller each,
/// and an abandoned frame still on the air counts as one) share the other `TX_SLOTS - 1`; more
/// callers park in a [`WaiterList`] until a slot frees.
const TX_SLOTS: usize = 4;
/// Callers that park while every slot is taken; one more re-polls itself until a slot frees.
const FULL_WAITERS: usize = 4;
/// Concurrent `receive` callers that park; one more re-polls itself until its turn.
const RECEIVE_WAITERS: usize = 4;
/// Wakers one locked operation can collect: every slot, every parked waiter, the scan gate.
const WAKE_CAPACITY: usize = TX_SLOTS + FULL_WAITERS + 3;

type TxResult = Result<(), EspHalEspNowError>;
type Wakes = Wakeups<WAKE_CAPACITY>;

/// Everything behind the one send-queue lock.
struct TxState {
    core: TxCore<TxResult, TX_SLOTS>,
    /// Callers waiting for a free slot ([`Refusal::Full`]).
    full_waiters: WaiterList<FULL_WAITERS>,
    /// The scan, waiting for the frame on the air to finish before its first radio action.
    idle_waiters: WaiterList<1>,
    /// A scan started right after a cancelled one, waiting for the cancelled scan's restore
    /// ([`GateRefusal::Unwinding`](juggler::espnow::GateRefusal::Unwinding)).
    unwind_waiters: WaiterList<1>,
    /// The scan exclusion of `core`, and the teardown of a scan that was cancelled while its
    /// probe was still on the air: the runner runs it right after that frame's completion, and
    /// only then ends the exclusion.
    /// The scan runs in its caller's future, so the job and its result carry nothing.
    scan: ScanGate<(), ()>,
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
    /// The TX power before a scan's burst boost, while the boost is in force.
    scan_saved_power: SharedCopy<Option<i8>>,
    /// The send queue: request slots, ids, deadlines, cancellation and scan exclusion.
    /// One lock, held for a few instructions and never across an await or a send.
    tx: Mutex<CriticalSectionRawMutex, RefCell<TxState>>,
    /// Wakes the runner after a submission (latched, so a wake-up is never lost).
    runner_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Wakes a scan's timed wait when [`EspHalEspNow::shutdown`] is called.
    shutdown_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Set while [`EspHalEspNow::run_sender`] is being polled.
    runner_active: SharedCopy<bool>,
    /// Serialises [`EspHalEspNow::receive`] callers: `esp-radio` keeps one receive waker.
    receive_gate: Gate<RECEIVE_WAITERS>,
    oversize: OversizeLog,
    /// The reply tap of the running scan: a `0xF4` reply from the probed peer goes here instead
    /// of to the application (ADR 025).
    reply: Mutex<CriticalSectionRawMutex, RefCell<ReplyMailbox>>,
    /// Wakes a scan that waits on the mailbox while an application `receive` holds the receiver.
    reply_wake: Signal<CriticalSectionRawMutex, ()>,
    /// Frames a scan read while it held the receiver; `receive` and `try_recv` drain them first.
    held: Mutex<CriticalSectionRawMutex, RefCell<HoldingBuffer>>,
    /// The channel the driver keeps pinned: the configured one at start, then the last scan's.
    pinned: SharedCopy<Option<u8>>,
    /// The probe sequence number the last scan ended on; the next scan continues it, so a
    /// reply left over from the previous scan never matches a new probe.
    scan_seq: SharedCopy<u8>,
    /// The channels a scan may probe: the radio's read-back of its country, taken at
    /// construction and on every [`EspHalEspNow::set_country`].
    allowed: SharedCopy<ChannelSet>,
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
    /// The controller keeps its country; the channels it reports bound later scans.
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
        // The caller's controller keeps its country; only read which channels it allows.
        let allowed = read_allowed();
        Ok(Self::assemble(
            esp_now,
            None,
            config.tx_power_quarter_dbm(),
            allowed,
            instance,
        ))
    }

    /// Starts the radio and ESP-NOW without any access point: an unassociated station that keeps
    /// its station MAC (ESP32-C3 matrix and nunchuk).
    ///
    /// Runs `WifiController::new` with A-MPDU RX/TX off; its default station configuration starts
    /// Wi-Fi without connecting; no SSID, no `embassy-net`, no executor needed.
    /// The radio runs under the country of `config` ([`EspNowConfig::with_country`]), else
    /// [`Country::WORLD`] (channels 1-11, 802.11d on), applied before the TX power and the
    /// channel; change it later with [`EspHalEspNow::set_country`].
    /// The TX power of `config` is applied, [`DEFAULT_TX_POWER_QUARTER_DBM`](crate::espnow::DEFAULT_TX_POWER_QUARTER_DBM) (13 dBm) when unset.
    /// The ESP-IDF `init_with_radio_config` takes the same [`EspNowConfig`] but leaves the driver
    /// default TX power when none is set.
    /// `esp_rtos::start` must have run and a heap must exist.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::AlreadyInitialised`] while another handle is alive.
    /// * [`EspHalEspNowError::InvalidTxPower`] / [`EspHalEspNowError::InvalidChannel`] for a bad
    ///   config, including a channel the configured country does not allow (12 or 13 under
    ///   [`Country::WORLD`]), refused before any radio work
    ///   ([`EspNowConfig::validate_owned`]).
    /// * [`EspHalEspNowError::Wifi`] if the driver fails to start.
    /// * [`EspHalEspNowError::Country`] if the driver rejects the country.
    /// * [`EspHalEspNowError::Radio`] if the radio itself refuses the configured channel.
    pub fn new_owned(
        wifi: esp_hal::peripherals::WIFI<'static>,
        config: EspNowConfig,
    ) -> Result<Self, EspHalEspNowError> {
        config.validate_owned()?;
        let instance =
            Singleton::acquire(&INSTANCE).ok_or(EspHalEspNowError::AlreadyInitialised)?;

        // The IDF brain sdkconfig disables both; Block-Ack exchanges starve ESP-NOW reception.
        let controller_config = ControllerConfig::default()
            .with_ampdu_rx_enable(false)
            .with_ampdu_tx_enable(false);
        let mut controller =
            WifiController::new(wifi, controller_config).map_err(EspHalEspNowError::Wifi)?;
        // `WifiController::new` applies esp-radio's own country (CN, channels 1-13); replace it
        // with the configured one (the world-safe default of both tiers when unset) before the
        // TX power and the channel.
        let country = config.owned_country();
        let applied =
            apply_country(&mut RadioCountry, country).map_err(EspHalEspNowError::Country)?;
        // `WifiController::new` leaves the esp-radio default of 20 (5 dBm); always set ours.
        let tx_power = config.owned_tx_power_quarter_dbm();
        controller
            .set_max_tx_power(tx_power)
            .map_err(EspHalEspNowError::Wifi)?;

        let esp_now = controller.esp_now();
        let handle = Self::assemble(
            esp_now,
            Some(controller),
            Some(tx_power),
            applied.channels,
            instance,
        );
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
        allowed: ChannelSet,
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
            scan_saved_power: SharedCopy::new(None),
            tx: Mutex::new(RefCell::new(TxState {
                core: TxCore::new(),
                full_waiters: WaiterList::new(),
                idle_waiters: WaiterList::new(),
                unwind_waiters: WaiterList::new(),
                scan: ScanGate::new(),
            })),
            runner_wake: Signal::new(),
            shutdown_wake: Signal::new(),
            runner_active: SharedCopy::new(false),
            receive_gate: Gate::new(),
            oversize: OversizeLog::new(),
            reply: Mutex::new(RefCell::new(ReplyMailbox::new())),
            reply_wake: Signal::new(),
            held: Mutex::new(RefCell::new(HoldingBuffer::new())),
            pinned: SharedCopy::new(None),
            scan_seq: SharedCopy::new(0),
            allowed: SharedCopy::new(allowed),
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

    /// Applies the Wi-Fi country to the owned radio and returns what the radio reports.
    ///
    /// [`EspHalEspNow::new_owned`] starts the radio under [`EspNowConfig::with_country`], else
    /// [`Country::WORLD`] (channels 1-11, 802.11d on); a scout that looks for a network on channel 12 or 13 must be set to that
    /// network's country, since an unassociated station cannot transmit outside its country
    /// (802.11d only affects an associated station).
    /// The channels the radio reports become the ones [`EspHalEspNow::scan_for_peer`] probes;
    /// call this before the first scan, a scan running meanwhile sees a forbidden channel as a
    /// failed channel change.
    /// Logs one line, e.g. `[wifi] country DE, channels 1-13, 802.11d on`.
    /// Same name and shape as `EspIdfEspNow::set_country` (ADR 022).
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::ControllerNotOwned`] on a [`EspHalEspNow::from_controller`] handle:
    ///   set the country on that controller (`wifi::set_country`).
    /// * [`EspHalEspNowError::Busy`] while another call holds the controller.
    /// * [`EspHalEspNowError::Country`] if the driver rejects the country.
    pub fn set_country(&self, country: Country) -> Result<Regulatory, EspHalEspNowError> {
        if !self.owns_controller {
            return Err(EspHalEspNowError::ControllerNotOwned);
        }
        let _controller = self.controller.lease().ok_or(EspHalEspNowError::Busy)?;
        let applied =
            apply_country(&mut RadioCountry, country).map_err(EspHalEspNowError::Country)?;
        self.allowed.set(applied.channels);
        Ok(applied)
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

    /// The primary Wi-Fi channel the radio is on right now, `None` when the radio is not
    /// started.
    ///
    /// Live radio state, unlike [`EspHalEspNow::pinned_channel`]: on a
    /// [`EspHalEspNow::from_controller`] handle it follows the access point, and during a scan
    /// it follows the probes.
    /// A coordinator puts it into its probe reply as `home_channel` (ADR 025).
    /// Read with the Wi-Fi driver's `esp_wifi_get_channel` (the query behind
    /// `WifiController::channel`), so it works on both constructors and needs no controller
    /// lease.
    /// Same name and shape as `EspIdfEspNow::current_channel` (ADR 022).
    pub fn current_channel(&self) -> Option<u8> {
        radio_channel()
    }

    /// Number of received frames the holding buffer dropped since start.
    ///
    /// A scan that read more than [`HOLD_CAPACITY`](super::HOLD_CAPACITY) application frames during one reply wait
    /// while no [`EspHalEspNow::receive`] was pending dropped the oldest ones; see the
    /// [module documentation](super).
    pub fn held_dropped(&self) -> u32 {
        self.held.lock(|held| held.borrow().dropped())
    }

    /// Arms the reply tap for a probe to `mac` that is about to be queued.
    fn arm_reply(&self, mac: MacAddress) {
        self.reply.lock(|mailbox| mailbox.borrow_mut().arm(mac));
        self.reply_wake.reset();
    }

    /// Disarms the reply tap: replies reach the application again.
    fn disarm_reply(&self) {
        self.reply.lock(|mailbox| mailbox.borrow_mut().disarm());
    }

    /// Offers a received frame to the reply tap; `true` when the tap took it, so the frame
    /// must not reach the application.
    fn offer_reply(&self, event: &EspNowEvent) -> bool {
        let taken = self
            .reply
            .lock(|mailbox| mailbox.borrow_mut().offer(&event.mac, event.payload()));
        if taken {
            self.reply_wake.signal(());
        }
        taken
    }

    fn take_reply(&self) -> Option<ProbeReply> {
        self.reply.lock(|mailbox| mailbox.borrow_mut().take())
    }

    /// Sets a frame the scan read aside for the application; a drop is logged like an
    /// oversize one (the first and then every 32nd).
    fn hold(&self, event: EspNowEvent) {
        let dropped = self.held.lock(|held| {
            let mut held = held.borrow_mut();
            held.push(event).then(|| held.dropped())
        });
        if let Some(dropped) = dropped {
            if OversizeLog::should_log(dropped) {
                log::warn!(
                    "ESP-NOW scan: holding buffer full, oldest application frame dropped, \
                     {dropped} dropped in total"
                );
            }
        }
    }

    fn pop_held(&self) -> Option<EspNowEvent> {
        self.held.lock(|held| held.borrow_mut().pop())
    }

    /// Sends and reports the MAC-layer ACK.
    ///
    /// Blocks for the TX duration like [`EspNowDriver::send`], then returns
    /// [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspHalEspNowError::Broadcast`] for the broadcast address: broadcast frames are never
    ///   acknowledged, use [`EspNowDriver::send`] (as on the ESP-IDF tier).
    /// * [`EspHalEspNowError::ScanInProgress`] while a [`EspHalEspNow::scan_for_peer`] runs.
    /// * [`EspHalEspNowError::Busy`] while another send is in flight (including the sender runner
    ///   mid-transmission).
    /// * [`EspHalEspNowError::SendFailed`] when a unicast frame was not acknowledged.
    /// * [`EspHalEspNowError::Radio`] if the driver rejects the frame (unknown peer, ...).
    pub fn send_and_wait(&self, mac: &MacAddress, data: &[u8]) -> Result<(), EspHalEspNowError> {
        let mut sender = self.lease_sender()?;
        self.admit_direct(mac, data, SendKind::App)?;
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
    /// Returns once [`EspHalEspNow::shutdown`] was called (after the frame on the air, if any,
    /// has finished); it never returns otherwise.
    ///
    /// Spawn it exactly once, in an `embassy_executor::task` that is never cancelled:
    ///
    /// ```ignore
    /// #[embassy_executor::task]
    /// async fn espnow_sender(espnow: &'static EspHalEspNow) {
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
    pub async fn run_sender(&self) {
        let Some(_active) = Singleton::acquire(&self.runner_active) else {
            log::error!("ESP-NOW: a sender runner is already running; this one does nothing");
            loop {
                core::future::pending::<()>().await;
            }
        };
        loop {
            if self.is_shut_down() {
                return;
            }
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

    /// Stops the driver's asynchronous machinery; idempotent, never blocks.
    ///
    /// Queued and in-flight requests end as [`EspHalEspNowError::Shutdown`], a running
    /// [`EspHalEspNow::scan_for_peer`] is aborted without a teardown (the channel, TX power and
    /// peer table stay as they are, `ScanError::Aborted`), every later send or scan is refused
    /// with the same error, and [`EspHalEspNow::run_sender`] returns after the frame on the air
    /// (if any) has finished; its result is ignored.
    /// Callers waiting for a free slot or for the air to clear are woken and fail the same way.
    /// Same name and semantics as `EspIdfEspNow::shutdown` (ADR 022).
    ///
    /// There is no `Drop` counterpart: every future of this handle borrows it, so the handle
    /// cannot be dropped while anything is pending, and a `'static` handle is never dropped.
    /// Call this method to stop it.
    pub fn shutdown(&self) {
        self.with_tx(|state, wakes| {
            state.core.shutdown(&mut |waker| wakes.push(waker));
            state.full_waiters.drain_into(wakes);
            state.idle_waiters.drain_into(wakes);
            state.unwind_waiters.drain_into(wakes);
        });
        self.runner_wake.signal(());
        self.shutdown_wake.signal(());
    }

    fn is_shut_down(&self) -> bool {
        self.with_tx(|state, _| state.core.is_shut_down())
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
    ///
    /// If the scan was cancelled while this frame was its probe on the air, the air is clear now:
    /// the scan gate hands back the deferred teardown, which runs here, and only then does the
    /// exclusion end.
    fn finish(&self, id: TxId, result: TxResult) {
        let (unknown, deferred) = self.with_tx(|state, wakes| {
            let unknown = match state.core.complete(id, result) {
                Completed::Delivered(waker) => {
                    if let Some(waker) = waker {
                        wakes.push(waker);
                    }
                    false
                }
                Completed::Discarded => false,
                Completed::Unknown => true,
            };
            state.idle_waiters.drain_into(wakes);
            state.full_waiters.drain_into(wakes);
            (unknown, state.scan.on_probe_complete(&state.core))
        });
        if unknown {
            log::warn!("ESP-NOW: send result for a frame that is not in flight dropped");
        }
        if let Some((epoch, teardown)) = deferred {
            self.run_teardown(teardown);
            self.release_scan(epoch);
        }
    }

    /// Ends the exclusion of scan `epoch`; call it only after its teardown has run.
    ///
    /// Also wakes a scan that waits for this one to unwind.
    fn release_scan(&self, epoch: JobEpoch) {
        self.with_tx(|state, wakes| {
            state.scan.release(&mut state.core, epoch);
            state.unwind_waiters.drain_into(wakes);
        });
    }

    /// Runs the teardown of a scan that can no longer drive its machine (a dropped future).
    ///
    /// Every step is a synchronous driver call (the ADR 020 exception for a drop path); `Abort`
    /// stands for each step's result, so no restore is skipped.
    fn run_teardown(&self, mut teardown: ScanTeardown) {
        let mac = *teardown.mac();
        let mut input = ScanInput::Abort;
        for _ in 0..ScanTeardown::MAX_POLLS {
            match teardown.poll(input) {
                ScanAction::Done(_) => return,
                // ADR 023 decision 5: the teardown had already finished; nothing to run.
                ScanAction::Ignored => {
                    log::error!("ESP-NOW scan: teardown ignored an input; giving up");
                    return;
                }
                action => input = self.perform_teardown_action(&mac, action),
            }
        }
        log::error!(
            "ESP-NOW scan: teardown not finished after {} steps",
            ScanTeardown::MAX_POLLS
        );
    }

    /// Performs one synchronous scan action for peer `mac` and reports its result.
    ///
    /// Shared by the scan loop and [`run_teardown`](Self::run_teardown).
    /// Probes and waits are not synchronous and never part of a teardown; they report `Failed`.
    fn perform_teardown_action(&self, mac: &MacAddress, action: ScanAction) -> ScanInput {
        match action {
            ScanAction::RemovePeer => completed(self.remove_peer(mac)),
            ScanAction::RegisterPeer { channel } => {
                let mut peer = PeerConfig::new(*mac);
                peer.channel = channel;
                completed(self.add_peer(&peer))
            }
            ScanAction::RegisterFallbackPeer { channel } => {
                let mut peer = PeerConfig::new(*mac);
                peer.channel = channel;
                let result = self.add_peer(&peer);
                if let Err(e) = &result {
                    log::warn!(
                        "ESP-NOW scan: could not fallback-register the peer on channel \
                         {channel}: {e:?}"
                    );
                }
                completed(result)
            }
            ScanAction::SetChannel { channel } => completed(self.set_channel(channel)),
            ScanAction::RestoreChannel { channel } => {
                let result = self.set_channel(channel);
                if let Err(e) = &result {
                    log::warn!("ESP-NOW scan: could not restore channel {channel}: {e:?}");
                }
                completed(result)
            }
            ScanAction::BoostTxPower => {
                // Advisory: without a controller the scan runs at the caller's level.
                self.boost_scan_power();
                ScanInput::Completed
            }
            ScanAction::RestoreTxPower => {
                self.restore_scan_power();
                ScanInput::Completed
            }
            ScanAction::SendProbe { .. }
            | ScanAction::WaitUntil(_)
            | ScanAction::AwaitReply { .. }
            | ScanAction::Done(_)
            | ScanAction::Ignored => ScanInput::Failed,
        }
    }

    /// Raises the TX power to [`TxPowerLevel::Max`] for a scan burst and saves the level before.
    ///
    /// A no-op without an owned controller or a known level, or while a boost is in force.
    fn boost_scan_power(&self) {
        if self.scan_saved_power.get().is_some() {
            return;
        }
        let Some(previous) = self.tx_power_quarter_dbm() else {
            return;
        };
        let max = TxPowerLevel::Max.to_quarter_dbm();
        if self.set_max_tx_power(max).is_ok() {
            self.scan_saved_power.set(Some(previous));
        }
    }

    /// Puts back the TX power saved by [`boost_scan_power`](Self::boost_scan_power), if any.
    fn restore_scan_power(&self) {
        let Some(previous) = self.scan_saved_power.get() else {
            return;
        };
        self.scan_saved_power.set(None);
        if self.set_max_tx_power(previous).is_err() {
            log::warn!("ESP-NOW scan: could not restore TX power to {previous} quarter dBm");
        }
        // The shadow value only says what this handle asked for; ask the radio.
        juggler::espnow::warn_on_restore_mismatch(Some(previous), radio_max_tx_power());
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
            if self.is_shut_down() {
                return Err(EspHalEspNowError::Shutdown);
            }
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
    /// waited out and its real result returned (`TxCore::poll_at` decides under the lock).
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
        if self.is_shut_down() {
            return Err(EspHalEspNowError::Shutdown);
        }
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
        let completion = match bounded(deadline, self.poll_completion(id)).await {
            Some(completion) => completion,
            // Deadline passed: `TxCore::poll_at` sees it in the request's own deadline, so a
            // queued request expires here and a frame on the air is waited out.
            // The timer fires at or after the deadline on the same clock, so the core agrees.
            None => self.poll_completion(id).await,
        };
        completion_result(completion)
    }

    async fn poll_completion(&self, id: TxId) -> Completion<TxResult> {
        poll_fn(|cx| {
            self.with_tx(|state, wakes| {
                let poll = state.core.poll_at(id, now_core(), cx.waker());
                if poll.is_ready() {
                    state.full_waiters.drain_into(wakes);
                }
                poll
            })
        })
        .await
    }

    /// Admits a sync send through [`TxCore::admit_direct`]: payload size, broadcast for
    /// [`SendKind::App`], and the scan exclusion, with the queue's own checks and order.
    ///
    /// Call it only while holding the sender lease: a scan waits for the sender to be free before
    /// its first radio action, so a send that leased first finishes before the scan touches the
    /// radio, and one that leased after the scan was admitted is refused here.
    fn admit_direct(
        &self,
        mac: &MacAddress,
        data: &[u8],
        kind: SendKind,
    ) -> Result<(), EspHalEspNowError> {
        self.with_tx(|state, _| state.core.admit_direct(*mac, data.len(), kind))
            .map_err(|refusal| refusal_error(refusal, data.len()))
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
    /// Fail-closed: the wait for a started frame's send callback has no limit, so a radio that
    /// never fires it blocks this call and every later transmission (the runner waits for ever)
    /// until the handle is dropped.
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
    /// waiting on a slot, queueing and the start of the transmission.
    /// A probe that has started is waited out and its real result returned; the deadline does not
    /// bound the ACK.
    /// A probe still queued at the deadline (with a zero timeout: at once) is never sent
    /// ([`EspHalEspNowError::Timeout`]); [`TxCore`] expires it.
    pub(crate) async fn send_probe(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), EspHalEspNowError> {
        validate_frame_len(data)?;
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
    ///
    /// Frames a running [`EspHalEspNow::scan_for_peer`] set aside (see the
    /// [module documentation](super)) come first, oldest first.
    /// A `0xF4` probe reply from the peer that scan probes is handed to the scan and never
    /// returned here (ADR 025).
    pub async fn receive(&self) -> EspNowEvent {
        let _turn = self.receive_gate.acquire().await;
        // The gate lets one call in at a time, so the lease is free unless a scan holds the
        // receiver for a reply wait (at most `reply_timeout` at a time); the frames that scan
        // sets aside are served meanwhile.
        let mut receiver = loop {
            match self.receiver.lease() {
                Some(lease) => break lease,
                None => {
                    if let Some(event) = self.pop_held() {
                        return event;
                    }
                    embassy_time::Timer::after_millis(RUNNER_LEASE_RETRY_MS).await;
                }
            }
        };
        // Only the holder of the receiver fills the holding buffer, so one look settles it.
        if let Some(event) = self.pop_held() {
            return event;
        }
        loop {
            let frame = receiver.receive_async().await;
            if let Some(event) = self.event_from(frame) {
                if self.offer_reply(&event) {
                    continue;
                }
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
    /// Cancelling the future (dropping it mid-scan) runs the machine's own teardown from the point
    /// it was dropped, synchronously in `Drop` (ADR 020 exception, `Drop` cannot await): the TX
    /// power is restored, the temporary peer is removed and, when a channel was pinned, the radio
    /// and the peer go back to it; only then does the exclusion end.
    /// If a probe is still on the air at that moment, `Drop` touches neither the channel, the TX
    /// power nor the peer table: the teardown waits in the scan gate for that frame's completion,
    /// the sender runner runs it right after, and the exclusion lasts until then (so application
    /// sends keep failing with `ScanInProgress` for the few milliseconds of the frame).
    /// A cancelled scan keeps the previous pin; a peer found on the way is only confirmed in the
    /// synchronous tail of the scan, where the future cannot be dropped.
    ///
    /// After an acknowledged probe the scan waits `config.reply_timeout` for the peer's `0xF4`
    /// reply, which names the peer's home channel and ends the scan there (ADR 025); an ACK
    /// without a reply keeps its old meaning as the fallback.
    /// With no [`EspHalEspNow::receive`] pending, the scan leases the receiver for that wait and
    /// sets every frame that is not its reply aside in a [`HOLD_CAPACITY`](super::HOLD_CAPACITY)-frame holding buffer
    /// (drop oldest, [`EspHalEspNow::held_dropped`]) that `receive` and `try_recv` drain first;
    /// a pending `receive` keeps the receiver and hands the reply to the scan instead.
    /// A reply to the outstanding probe never reaches the application; the tap is armed from a
    /// probe's send until the scan moves past its reply wait, so a `0xF4` that arrives between
    /// probes (a late or MAC-retransmitted one) is handed on like any frame.
    ///
    /// `config.probe_timeout` bounds queueing and the start of each probe: a probe that did not
    /// start in time is never sent and counts as a local failure (not as "no ACK").
    /// It does not bound the ACK: a probe that has started is waited out and its real MAC-layer
    /// result used (transmitted but unacknowledged is "no ACK").
    /// A cancelled or timed-out probe never orphans a completion.
    /// The scan never changes channel while a probe is being transmitted: a probe that has started
    /// is waited out (the burst budget counts the overrun, the machine is fed the real time), and
    /// one that has not started is dropped by the runner.
    /// That wait is fail-closed: if the radio never fires the send callback of a started probe, the
    /// runner waits for ever, and with it this scan and every later transmission, until the handle
    /// is dropped.
    /// No lock is held across an await point.
    ///
    /// # Errors
    ///
    /// * [`EspHalEspNowError::ControllerNotOwned`] on a [`EspHalEspNow::from_controller`] handle.
    /// * [`EspHalEspNowError::Busy`] while another caller's `scan_for_peer` is running or its
    ///   result is untaken; scans are serialized.
    ///   A scan started right after a cancelled one does not fail: it waits for the cancelled
    ///   scan's restore (one probe on the air plus the teardown) and then runs.
    /// * [`EspHalEspNowError::Shutdown`] after [`EspHalEspNow::shutdown`], also while waiting
    ///   for a cancelled scan's restore.
    /// * [`EspHalEspNowError::Scan`] with the [`ScanError`](juggler::espnow::ScanError) reported
    ///   by the machine.
    pub async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, EspHalEspNowError> {
        // A scan cancelled just before still unwinds (probe on the air, restore pending): wait
        // for `release_scan` (or shutdown) to wake us, then admit again.
        let epoch = poll_fn(|cx| {
            self.with_tx(|state, wakes| {
                let admitted =
                    state
                        .scan
                        .admit(&mut state.core, self.owns_controller, (), &mut |waker| {
                            wakes.push(waker)
                        });
                match admitted {
                    Ok(epoch) => Poll::Ready(Ok(epoch)),
                    Err((GateRefusal::Unwinding, ())) => {
                        if !state.unwind_waiters.register(cx.waker()) {
                            cx.waker().wake_by_ref();
                        }
                        Poll::Pending
                    }
                    Err((refusal, ())) => Poll::Ready(Err(gate_refusal_error(refusal))),
                }
            })
        })
        .await?;
        let mut machine = ScanMachine::new(*mac, *config, self.pinned.get())
            .with_first_seq(self.scan_seq.get())
            .with_allowed_channels(self.allowed.get());
        // Holds the exclusion and, on drop, runs the teardown of the machine's last state
        // (immediately, or after the probe on the air when the future was cancelled mid-probe).
        let mut guard = ScanGuard {
            radio: self,
            epoch,
            teardown: machine.abort_teardown(),
            finished: false,
        };
        self.wait_scan_start().await;
        let start = embassy_time::Instant::now();
        let mut input = ScanInput::Start;
        let result = loop {
            let now = Duration::from_micros(start.elapsed().as_micros());
            // Shutdown ends the scan at once without a teardown, like the ESP-IDF worker.
            if input != ScanInput::Start && self.is_shut_down() {
                input = ScanInput::Shutdown;
            }
            let action = match machine.poll_bounded(now, input) {
                Ok(action) => action,
                Err(e) => break Err(e),
            };
            guard.teardown = machine.abort_teardown();
            // The reply tap is armed from a probe's send until the scan moves past its reply
            // wait: the reply can arrive right behind the ACK, before `AwaitReply` is issued.
            match action {
                ScanAction::SendProbe { .. } => self.arm_reply(*mac),
                ScanAction::AwaitReply { .. } => {}
                _ => self.disarm_reply(),
            }
            input = match action {
                ScanAction::SetChannel { channel } => {
                    let result = self.set_channel(channel);
                    if let Err(e) = &result {
                        machine.log_fault(format_args!("failed to set channel {channel}: {e:?}"));
                    }
                    completed(result)
                }
                ScanAction::SendProbe {
                    channel, timeout, ..
                } => {
                    let result = self
                        .send_probe(mac, &machine.probe_frame(channel), timeout)
                        .await;
                    let outcome = SendOutcome::of(&result);
                    if let (SendOutcome::Local, Err(e)) = (outcome, &result) {
                        machine.log_fault(format_args!("probe on channel {channel} failed: {e:?}"));
                    }
                    ScanInput::Sent(outcome)
                }
                ScanAction::WaitUntil(deadline) => {
                    let _ = embassy_time::with_deadline(
                        start + to_embassy(deadline),
                        self.shutdown_wake.wait(),
                    )
                    .await;
                    ScanInput::Completed
                }
                ScanAction::AwaitReply { until } => {
                    self.await_reply(start + to_embassy(until)).await
                }
                ScanAction::Done(result) => break result,
                // ADR 023 decision 5: the loop breaks at `Done`, so a no-op answer means the
                // machine was polled past its end, a driver bug.
                ScanAction::Ignored => {
                    log::error!("ESP-NOW scan: the machine ignored an input; giving up");
                    break Err(ScanError::UnexpectedInput);
                }
                action => self.perform_teardown_action(mac, action),
            };
        };
        guard.finish();
        self.pinned.set(machine.pinned_channel());
        self.scan_seq.set(machine.seq());
        result.map_err(EspHalEspNowError::Scan)
    }

    /// Waits for the probed peer's reply until `deadline`: [`ScanInput::Reply`] for one,
    /// [`ScanInput::Completed`] once the deadline passed or on shutdown.
    ///
    /// The tap is armed for the peer since the probe was queued, so a reply that arrived right
    /// behind the ACK is already in the mailbox.
    /// With no application `receive` pending the scan leases the receiver and reads the frames
    /// itself, setting every frame that is not its reply aside in the holding buffer; while a
    /// `receive` holds the receiver, that call feeds the mailbox and the scan waits on it, and
    /// retries the lease every [`RUNNER_LEASE_RETRY_MS`]: that `receive` may return an
    /// application frame (or be dropped) and leave the reply unread in `esp-radio`'s queue.
    /// The deadline is checked before the mailbox, so a peer that streams replies the machine
    /// ignores cannot hold the scan in its wait past `until`.
    /// Cancel-safe: dropping the future returns the receiver, and the frames set aside stay for
    /// the application.
    async fn await_reply(&self, deadline: embassy_time::Instant) -> ScanInput {
        loop {
            if expired(Some(deadline), embassy_time::Instant::now()) {
                return ScanInput::Completed;
            }
            if let Some(reply) = self.take_reply() {
                return ScanInput::Reply(reply);
            }
            let Some(mut receiver) = self.receiver.lease() else {
                let woken = embassy_time::with_deadline(
                    deadline,
                    select3(
                        self.reply_wake.wait(),
                        self.shutdown_wake.wait(),
                        embassy_time::Timer::after_millis(RUNNER_LEASE_RETRY_MS),
                    ),
                )
                .await;
                match woken {
                    Ok(Either3::First(())) | Ok(Either3::Third(())) => continue,
                    Ok(Either3::Second(())) | Err(_) => return ScanInput::Completed,
                }
            };
            let read = async {
                loop {
                    let frame = receiver.receive_async().await;
                    let Some(event) = self.event_from(frame) else {
                        continue;
                    };
                    if self.offer_reply(&event) {
                        if let Some(reply) = self.take_reply() {
                            return reply;
                        }
                        continue;
                    }
                    self.hold(event);
                }
            };
            return match embassy_time::with_deadline(
                deadline,
                select(read, self.shutdown_wake.wait()),
            )
            .await
            {
                Ok(Either::First(reply)) => ScanInput::Reply(reply),
                Ok(Either::Second(())) | Err(_) => ScanInput::Completed,
            };
        }
    }

    /// Waits until no frame is on the air and picks the scan up, then until no sync send holds
    /// the sender, so the scan's first radio action cannot change the channel under a
    /// transmission.
    async fn wait_scan_start(&self) {
        poll_fn(|cx| {
            self.with_tx(|state, _| {
                if state.scan.may_start(&state.core) {
                    let _ = state.scan.pickup();
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
        let mut sender = self.lease_sender()?;
        self.admit_direct(mac, data, SendKind::Unacked)?;
        let waiter = sender.send(mac, data).map_err(map_error)?;
        // The ACK outcome is deliberately ignored, exactly like the IDF `send`.
        let _ = waiter.wait();
        Ok(())
    }

    /// Frames a scan set aside come first; a probe reply the scan waits for is handed to it and
    /// skipped (see [`EspHalEspNow::receive`]).
    fn try_recv(&self) -> Option<EspNowEvent> {
        if let Some(event) = self.pop_held() {
            return Some(event);
        }
        for _ in 0..RX_SKIP_LIMIT {
            // Pop under the lock, convert (and free the frame) outside it.
            let frame = self.receiver.with_locked(|r| r.receive()).flatten()?;
            if let Some(event) = self.event_from(frame) {
                if self.offer_reply(&event) {
                    continue;
                }
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

    fn current_channel(&self) -> Option<u8> {
        EspHalEspNow::current_channel(self)
    }

    fn shutdown(&self) {
        EspHalEspNow::shutdown(self);
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
    // The inherent `shutdown` keeps the name and shape of the ESP-IDF tier's (ADR 022).
    let _: fn(&EspHalEspNow) = EspHalEspNow::shutdown;
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

/// Holds the scan exclusion of one `scan_for_peer` call and puts the radio back when the call
/// is dropped before its machine finished.
///
/// `teardown` is the machine's [`ScanMachine::abort_teardown`], refreshed after every poll.
/// On a normal finish the machine has restored everything itself, and
/// [`ScanGuard::finish`] only ends the exclusion.
/// When the future is dropped mid-scan, the scan gate decides under the queue lock: with the
/// air clear the teardown runs synchronously here (ADR 020 exception, `Drop` cannot await);
/// with the scan's probe still on the air it waits in the gate and the runner runs it right
/// after that frame's completion.
/// The exclusion ends only once the teardown has run, in both cases.
///
/// A scan that was cancelled keeps the previous pin.
/// The peer is only confirmed after the last probe, and everything after that is synchronous
/// (no await), so a dropped future never has a found peer to keep.
struct ScanGuard<'a> {
    radio: &'a EspHalEspNow,
    epoch: JobEpoch,
    teardown: ScanTeardown,
    finished: bool,
}

impl ScanGuard<'_> {
    /// The machine finished: end the exclusion.
    fn finish(mut self) {
        self.finished = true;
        self.radio.disarm_reply();
        self.radio.release_scan(self.epoch);
    }
}

impl Drop for ScanGuard<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        // Whatever ends the scan, the reply tap must not outlive it.
        self.radio.disarm_reply();
        let outcome = self.radio.with_tx(|state, _| {
            state
                .scan
                .cancel_running(&mut state.core, self.epoch, self.teardown)
        });
        // Not picked up yet: the gate already ended the exclusion. Deferred: the runner runs
        // the teardown and ends it.
        if let CancelOutcome::RunNow(teardown) = outcome {
            self.radio.run_teardown(teardown);
            self.radio.release_scan(self.epoch);
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

/// Reads the radio's actual maximum TX power in quarter dBm, `None` when the driver refuses.
fn radio_max_tx_power() -> Option<i8> {
    extern "C" {
        fn esp_wifi_get_max_tx_power(power: *mut i8) -> i32;
    }
    let mut power: i8 = 0;
    // SAFETY: `esp_wifi_get_max_tx_power` is provided by esp-wifi-sys, always linked by every
    // esp-radio build, and writes the current level to the valid `&mut i8` passed in.
    // It fails with an error code (read as `None`) when Wi-Fi is not started.
    let rc = unsafe { esp_wifi_get_max_tx_power(&mut power) };
    (rc == 0).then_some(power)
}

/// The channels the radio's country allows; every channel when the read fails, so the scan
/// keeps its old behaviour and a forbidden channel surfaces as a failed channel change.
fn read_allowed() -> ChannelSet {
    match RadioCountry.country() {
        Ok(regulatory) => regulatory.channels,
        Err(rc) => {
            log::warn!(
                "ESP-NOW: reading the Wi-Fi country failed ({rc:#x}); scans probe every channel"
            );
            ChannelSet::ALL
        }
    }
}

/// Reads the primary channel the radio is on, `None` when the driver refuses (not started).
///
/// The same `esp_wifi_get_channel` query that `WifiController::channel` runs, callable without
/// the controller: a handle from `from_controller` never holds one.
fn radio_channel() -> Option<u8> {
    extern "C" {
        fn esp_wifi_get_channel(primary: *mut u8, second: *mut u32) -> i32;
    }
    let mut primary: u8 = 0;
    let mut second: u32 = 0;
    // SAFETY: `esp_wifi_get_channel` is provided by esp-wifi-sys, always linked by every
    // esp-radio build, and writes the primary channel and the secondary-channel enum to the two
    // valid locals passed in. It fails with an error code (read as `None`) when Wi-Fi is not
    // started.
    let rc = unsafe { esp_wifi_get_channel(&mut primary, &mut second) };
    (rc == 0).then_some(primary)
}
