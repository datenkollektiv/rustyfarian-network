//! ESP-IDF driver implementing [`EspNowDriver`] via `esp-idf-svc`.
//!
//! # Usage
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_espnow::EspIdfEspNow;
//! use espnow_pure::{EspNowDriver, PeerConfig};
//!
//! let driver = EspIdfEspNow::init().unwrap();
//! let config = PeerConfig::new([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
//! driver.add_peer(&config).unwrap();
//! driver.send(&config.mac, b"hello").unwrap();
//! ```

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo};
pub use espnow_pure::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, BROADCAST_MAC, DEFAULT_RX_CHANNEL_CAPACITY,
    MAX_DATA_LEN,
};

/// ESP-IDF implementation of [`EspNowDriver`].
///
/// Wraps [`EspNow<'static>`] and bridges the C receive callback into a
/// [`std::sync::mpsc::sync_channel`] for non-blocking polling.
pub struct EspIdfEspNow {
    esp_now: EspNow<'static>,
    rx: Receiver<EspNowEvent>,
}

impl EspIdfEspNow {
    /// Initialise ESP-NOW with the default receive-queue capacity of
    /// [`DEFAULT_RX_CHANNEL_CAPACITY`] frames.
    pub fn init() -> anyhow::Result<Self> {
        Self::init_with_capacity(DEFAULT_RX_CHANNEL_CAPACITY)
    }

    /// Initialise ESP-NOW with a custom receive-queue capacity.
    ///
    /// Frames received while the queue is full are dropped with a warning log.
    pub fn init_with_capacity(capacity: usize) -> anyhow::Result<Self> {
        let esp_now = EspNow::take().context("failed to acquire EspNow singleton")?;

        let (tx, rx): (SyncSender<EspNowEvent>, Receiver<EspNowEvent>) = sync_channel(capacity);

        esp_now
            .register_recv_cb(move |info, data| {
                let mac = *info.src_addr;
                let event = EspNowEvent::new(mac, data);
                if tx.try_send(event).is_err() {
                    log::warn!(
                        "ESP-NOW receive queue full — frame from {:02X?} dropped",
                        mac
                    );
                }
            })
            .context("failed to register ESP-NOW receive callback")?;

        Ok(Self { esp_now, rx })
    }
}

impl EspNowDriver for EspIdfEspNow {
    type Error = anyhow::Error;

    fn add_peer(&self, config: &PeerConfig) -> anyhow::Result<()> {
        let peer_info = PeerInfo {
            peer_addr: config.mac,
            channel: config.channel,
            encrypt: config.encrypt,
            ..Default::default()
        };
        self.esp_now
            .add_peer(peer_info)
            .context("failed to add ESP-NOW peer")
    }

    fn remove_peer(&self, mac: &MacAddress) -> anyhow::Result<()> {
        self.esp_now
            .del_peer(*mac)
            .context("failed to remove ESP-NOW peer")
    }

    fn send(&self, mac: &MacAddress, data: &[u8]) -> anyhow::Result<()> {
        espnow_pure::validate_payload(data)
            .map_err(|e| anyhow::anyhow!(e))
            .context("payload validation failed")?;
        self.esp_now
            .send(*mac, data)
            .context("failed to send ESP-NOW frame")
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        self.rx.try_recv().ok()
    }
}
