//! OTAA join test for Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262) using ESP-IDF.
//!
//! # Credentials
//!
//! Set at build time via environment variables:
//!
//! ```sh
//! LORAWAN_DEV_EUI=0000000000000001 \
//! LORAWAN_APP_EUI=0000000000000002 \
//! LORAWAN_APP_KEY=00000000000000000000000000000003 \
//! just build-example idf_esp32s3_join
//! ```
//!
//! EUIs are 16 hex chars (8 bytes) MSB-first as shown in TTN Console.
//! AppKey is 32 hex chars (16 bytes).
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

    let mut response = device
        .join(join_mode)
        .map_err(|e| anyhow::anyhow!("{:?}", e))?;

    let start = Instant::now();
    let mut next_timeout_ms: u32 = 0;
    if let Response::TimeoutRequest(ms) = response {
        next_timeout_ms = start.elapsed().as_millis() as u32 + ms;
        response = Response::NoUpdate;
    }

    loop {
        let elapsed_ms = start.elapsed().as_millis() as u32;

        // DIO1 fired — deliver radio event to the lorawan-device state machine.
        if DIO1_FLAG.swap(false, Ordering::AcqRel) {
            log::info!(target: tag, "DIO1 at {}ms", elapsed_ms);
            response = device
                .handle_event(Event::RadioEvent(LdRadioEvent::Phy(())))
                .map_err(|e| anyhow::anyhow!("{:?}", e))?;
        }

        // Timing tick — deliver timeout when the deadline has elapsed.
        if elapsed_ms >= next_timeout_ms && next_timeout_ms > 0 {
            let late_ms = elapsed_ms.saturating_sub(next_timeout_ms);
            log::info!(
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
                next_timeout_ms = elapsed_ms + ms;
                response = Response::NoUpdate;
            }
            Response::JoinRequestSending => {
                log::info!(target: tag, "Join-request on air ...");
                response = Response::NoUpdate;
            }
            Response::NoUpdate => {}
            other => {
                log::info!(target: tag, "response: {:?}", other);
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
