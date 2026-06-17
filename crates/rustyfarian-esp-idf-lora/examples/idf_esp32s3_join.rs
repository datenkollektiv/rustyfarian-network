//! OTAA join test for Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262) using ESP-IDF.
//!
//! # Credentials
//!
//! LoRaWAN credentials are loaded at build time from environment variables:
//! `LORAWAN_DEV_EUI`, `LORAWAN_APP_EUI`, and `LORAWAN_APP_KEY`.
//!
//! ## Setup (recommended: .env workflow)
//!
//! 1. Copy TTN Console credentials (MSB-first as shown there):
//!    - DevEUI: 16 hex chars (8 bytes)
//!    - AppEUI (JoinEUI): 16 hex chars (8 bytes)
//!    - AppKey: 32 hex chars (16 bytes)
//!
//! 2. Fill in the `.env` file at the workspace root:
//!
//! ```sh
//! LORAWAN_DEV_EUI=0123456789ABCDEF
//! LORAWAN_APP_EUI=0000000000000000
//! LORAWAN_APP_KEY=00112233445566778899AABBCCDDEEFF
//! ```
//!
//! 3. Build and flash:
//!
//! ```sh
//! just run idf_esp32s3_join
//! ```
//!
//! ## Alternative: inline environment variables
//!
//! Override `.env` by passing credentials inline (takes precedence):
//!
//! ```sh
//! LORAWAN_DEV_EUI=0000000000000001 \
//! LORAWAN_APP_EUI=0000000000000002 \
//! LORAWAN_APP_KEY=00000000000000000000000000000003 \
//! just run idf_esp32s3_join
//! ```
//!
//! # Expected output
//!
//! On a successful join:
//!
//! ```text
//! I (NNN) idf_esp32s3_join: SX1262 initialized
//! I (NNN) idf_esp32s3_join: Sending OTAA join-request ...
//! I (NNN) idf_esp32s3_join: OTAA join successful
//! ```
//!
//! Check TTN v3 Live Data for "forward join-request" and "accept join-request".
//!
//! # DIO1 interrupt
//!
//! GPIO 14 is set up with a rising-edge interrupt that sets `DIO1_FLAG`.
//! The main loop polls this flag and delivers `RadioEvent(Phy(()))` to the
//! lorawan-device state machine when it fires.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use esp_idf_hal::{
    gpio::{InterruptType, PinDriver, Pull},
    spi::{SpiConfig, SpiDeviceDriver, SpiDriver, SpiDriverConfig},
    units::FromValueType,
};
use esp_idf_svc::hal::peripherals::Peripherals;

use lorawan_device::{
    nb_device::{radio::Event as LdRadioEvent, Device, Event, Response},
    region, AppEui, AppKey, DevEui, JoinMode,
};

use rustyfarian_esp_idf_lora::{
    config::{LoraConfig, Region},
    sx1262_driver::{EspIdfLoraRadio, LoraRadioAdapter},
};

/// DIO1 rising-edge flag — set by ISR, cleared by main loop after delivery.
static DIO1_FLAG: AtomicBool = AtomicBool::new(false);

/// Data rate used for the OTAA join.
///
/// `DR5` (SF7/BW125) is chosen for **local-gateway validation**: an SF12 (`DR0`,
/// the EU868 default) join-accept is ~1.8 s of airtime, but lorawan-device hard-caps
/// the RX1 window at the RX1→RX2 gap (`min(duration, 1000 ms)` for EU868 OTAA), so an
/// SF12 accept can never complete inside RX1. At SF7 the accept is ~70 ms and fits RX1
/// with margin. RX2 stays SF12/869.525 and is covered by `RX_WINDOW_DURATION_MS`.
///
/// This bakes in a short-range assumption (a strong local link). For **range testing**,
/// prefer `DR0` (SF12) and ensure the gateway schedules the accept on RX2, or accept that
/// RX1 will not complete at long range.
const JOIN_DR: region::DR = region::DR::_5;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let tag = "idf_esp32s3_join";

    // ─── Credentials (compile-time env vars) ──────────────────────────────────

    let dev_eui_hex = option_env!("LORAWAN_DEV_EUI").unwrap_or("0000000000000000");
    let app_eui_hex = option_env!("LORAWAN_APP_EUI").unwrap_or("0000000000000000");
    let app_key_hex = option_env!("LORAWAN_APP_KEY").unwrap_or("00000000000000000000000000000000");

    let lora_config =
        LoraConfig::from_hex_strings(Region::EU868, dev_eui_hex, app_eui_hex, app_key_hex)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "invalid LoRaWAN credentials — check LORAWAN_DEV_EUI/APP_EUI/APP_KEY"
                )
            })?;

    log::info!(target: tag, "DevEUI loaded (MSB-first)");

    // ─── Peripherals ──────────────────────────────────────────────────────────

    let peripherals = Peripherals::take()?;

    // ─── DIO1 interrupt (GPIO 14) ─────────────────────────────────────────────

    // Floating: DIO1 is driven by the SX1262 — no internal pull needed.
    let mut dio1 = PinDriver::input(peripherals.pins.gpio14, Pull::Floating)?;
    dio1.set_interrupt_type(InterruptType::PosEdge)?;
    // SAFETY: the ISR closure captures only `DIO1_FLAG` which is `'static`.
    // No data race: `AtomicBool` with `Release`/`Acquire` ordering.
    unsafe {
        dio1.subscribe(|| {
            DIO1_FLAG.store(true, Ordering::Release);
        })?;
    }
    // `enable_interrupt()` is called once.  ESP-IDF edge interrupts re-arm
    // automatically after each firing — no explicit re-enable is needed in
    // the main loop.  If only a single DIO1 event is ever observed, check
    // that the `subscribe` closure is still alive (it is here, captured via
    // the `'static` atomic) and that `dio1` was not dropped prematurely.
    dio1.enable_interrupt()?;

    // ─── SPI2 bus (8 MHz, mode 0) ─────────────────────────────────────────────

    let spi_driver = SpiDriver::new(
        peripherals.spi2,
        peripherals.pins.gpio9,        // SCK
        peripherals.pins.gpio10,       // MOSI
        Some(peripherals.pins.gpio11), // MISO
        &SpiDriverConfig::new(),
    )?;

    let spi_device = SpiDeviceDriver::new(
        spi_driver,
        Some(peripherals.pins.gpio8), // NSS/CS
        &SpiConfig::new()
            .baudrate(8.MHz().into())
            .data_mode(embedded_hal::spi::MODE_0),
    )?;

    // ─── Radio pins ───────────────────────────────────────────────────────────

    let rst = PinDriver::output(peripherals.pins.gpio12)?; // RST — active low
    let busy = PinDriver::input(peripherals.pins.gpio13, Pull::Floating)?; // BUSY — driven by SX1262
    let ant = PinDriver::output(peripherals.pins.gpio0)?; // spare GPIO; DIO2 controls RF switch

    // ─── EspIdfLoraRadio ──────────────────────────────────────────────────────
    // `dio1` is moved here; the ISR subscription remains active because the
    // closure only captures `DIO1_FLAG` (a `'static`), not the `PinDriver`.

    let radio = EspIdfLoraRadio::new(spi_device, rst, busy, ant, dio1, &lora_config)
        .map_err(|e| anyhow::anyhow!("SX1262 init failed — check SPI wiring and TCXO: {:?}", e))?;

    log::info!(target: tag, "SX1262 initialized");

    // ─── LoRaWAN device ───────────────────────────────────────────────────────

    let adapter = LoraRadioAdapter::new(radio);
    let region = region::Configuration::new(region::Region::EU868);
    let mut device: Device<_, lorawan_device::default_crypto::DefaultFactory, _, 256> =
        Device::new(region, adapter, EspRng);

    // ─── OTAA join ────────────────────────────────────────────────────────────

    log::info!(target: tag, "Sending OTAA join-request ...");

    // TTN Console shows EUIs MSB-first; lorawan-device expects LSB-first.
    let mut dev_eui_bytes = lora_config.dev_eui;
    let mut app_eui_bytes = lora_config.app_eui;
    dev_eui_bytes.reverse();
    app_eui_bytes.reverse();

    let join_mode = JoinMode::OTAA {
        deveui: DevEui::from(dev_eui_bytes),
        appeui: AppEui::from(app_eui_bytes),
        appkey: AppKey::from(lora_config.app_key),
    };

    // See `JOIN_DR` for why DR5 (SF7/BW125) is used for local-gateway validation
    // and when DR0 (SF12) is preferable for range testing.
    device.set_datarate(JOIN_DR);
    log::info!(target: tag, "Join data rate set to {:?} (DR5 = SF7/BW125)", JOIN_DR);

    let mut response = device
        .join(join_mode)
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let start = Instant::now();
    let mut next_timeout_ms: u32 = 0;
    if let Response::TimeoutRequest(ms) = response {
        // `TimeoutRequest(ms)` carries an ABSOLUTE timestamp in the device's
        // millisecond timeline (epoch ≈ TX start ≈ `start`), NOT a relative
        // delay. Assigning it directly (rather than `elapsed + ms`) is what
        // makes RX1/RX2 open on time; adding `elapsed` opens them ~2x too late
        // and the join-accept is missed. See lorawan-device nb_device::Response.
        next_timeout_ms = ms;
        log::debug!(target: tag, "RX window scheduled at {}ms (absolute)", ms);
        response = Response::NoUpdate;
    }

    loop {
        let elapsed_ms = start.elapsed().as_millis() as u32;

        // Deliver a radio event when the SX1262 has a pending IRQ.
        //
        // We do NOT rely on the DIO1 GPIO interrupt alone: esp-idf-hal PinDriver
        // interrupts are one-shot (auto-disabled on each firing, must be re-armed
        // from a non-ISR context), and the DIO1 pin is owned by the radio driver
        // so it cannot be re-armed here. The interrupt therefore only ever
        // delivers the first edge (TxDone); RxDone was being missed entirely.
        // Polling the radio's IRQ register over SPI each tick is robust and
        // catches every RxDone/Timeout. The DIO1 flag is still consumed so a
        // delivered edge is not left pending.
        let dio1_edge = DIO1_FLAG.swap(false, Ordering::AcqRel);
        if dio1_edge || device.get_radio().irq_pending() {
            log::debug!(target: tag, "radio IRQ at {}ms", elapsed_ms);
            response = device
                .handle_event(Event::RadioEvent(LdRadioEvent::Phy(())))
                .map_err(|e| anyhow::anyhow!("{:?}", e))?;
        }

        // Timing tick — deliver timeout when the deadline has elapsed.
        if elapsed_ms >= next_timeout_ms && next_timeout_ms > 0 {
            let late_ms = elapsed_ms.saturating_sub(next_timeout_ms);
            log::debug!(
                target: tag,
                "Timeout fired at {}ms (deadline {}ms, {}ms late)",
                elapsed_ms, next_timeout_ms, late_ms
            );
            next_timeout_ms = 0;
            response = device
                .handle_event(Event::TimeoutFired)
                .map_err(|e| anyhow::anyhow!("{:?}", e))?;
        }

        match &response {
            Response::JoinSuccess => {
                log::info!(target: tag, "OTAA join successful");
                break;
            }
            Response::NoJoinAccept => {
                log::error!(target: tag, "OTAA join failed — no join-accept");
                break;
            }
            Response::TimeoutRequest(ms) => {
                // Absolute timestamp in the device timeline — assign, don't add.
                next_timeout_ms = *ms;
                log::debug!(target: tag, "RX window scheduled at {}ms (absolute)", *ms);
                response = Response::NoUpdate;
            }
            Response::JoinRequestSending => {
                log::info!(target: tag, "Join-request on air ...");
                response = Response::NoUpdate;
            }
            Response::NoUpdate => {}
            other => {
                log::debug!(target: tag, "response: {:?}", other);
                response = Response::NoUpdate;
            }
        }

        // Safety net — abort after 60 s.
        if elapsed_ms > 60_000 {
            log::error!(target: tag, "Join timed out after 60 s");
            break;
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    Ok(())
}

// ─── EspRng — hardware RNG wrapper ────────────────────────────────────────────

struct EspRng;

impl rand_core::RngCore for EspRng {
    fn next_u32(&mut self) -> u32 {
        // SAFETY: `esp_random()` reads the hardware RNG — no shared mutable state.
        unsafe { esp_idf_svc::sys::esp_random() }
    }

    fn next_u64(&mut self) -> u64 {
        let hi = self.next_u32() as u64;
        let lo = self.next_u32() as u64;
        (hi << 32) | lo
    }

    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(4) {
            let val = self.next_u32().to_le_bytes();
            for (d, s) in chunk.iter_mut().zip(val.iter()) {
                *d = *s;
            }
        }
    }

    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}
