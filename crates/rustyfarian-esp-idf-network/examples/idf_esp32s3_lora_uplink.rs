//! OTAA join + periodic temperature uplink for Heltec WiFi LoRa 32 V3 (ESP32-S3 + SX1262).
//!
//! Joins TTN via OTAA (same flow as `idf_esp32s3_join`), then sends the ESP32-S3 internal
//! die-temperature reading as an unconfirmed uplink every 30 s on fPort 1.
//!
//! # Payload format
//!
//! 2 bytes, big-endian signed 16-bit integer: `(temp_c * 100.0).round() as i16`.
//! Decoder: `temp = int16 / 100` (result in °C).
//!
//! ## Observing the data in TTN
//!
//! Without a payload formatter, TTN Live Data shows only the raw `frm_payload`
//! (e.g. `0E7D`) — the decoded temperature does NOT appear. To see
//! `temperature_c` you MUST add the decoder below:
//!
//! 1. TTN Console > your Application > **Payload formatters** > **Uplink**.
//! 2. Set **Formatter type** to **Custom JavaScript formatter** (the default is
//!    "None", which leaves the payload undecoded).
//! 3. Paste the function below into the **Formatter code** box and **Save changes**.
//!
//! The decoded `temperature_c` field then appears in Live Data and is forwarded
//! to any integrations (MQTT, webhooks, storage).
//!
//! ```js
//! function decodeUplink(input) {
//!   var raw = (input.bytes[0] << 8) | input.bytes[1];
//!   if (raw > 32767) raw -= 65536;
//!   return { data: { temperature_c: raw / 100 } };
//! }
//! ```
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
//! just run idf_esp32s3_lora_uplink
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
//! just run idf_esp32s3_lora_uplink
//! ```
//!
//! # Expected output
//!
//! ```text
//! I (NNN) idf_esp32s3_lora_uplink: SX1262 initialized
//! I (NNN) idf_esp32s3_lora_uplink: Sending OTAA join-request ...
//! I (NNN) idf_esp32s3_lora_uplink: OTAA join successful
//! I (NNN) idf_esp32s3_lora_uplink: Die temperature: 42.50 °C
//! I (NNN) idf_esp32s3_lora_uplink: Sending uplink: temp=4250 (0x10, 0x9A)
//! I (NNN) idf_esp32s3_lora_uplink: Uplink complete (UplinkSending)
//! ```
//!
//! # Radio event source (DIO1 vs SPI polling)
//!
//! The **primary** event source is SPI polling: each loop tick calls
//! `device.get_radio().irq_pending()` (a `GetIrqStatus` read) and delivers
//! `RadioEvent(Phy(()))` when a relevant IRQ is latched. The GPIO 14 DIO1
//! rising-edge interrupt (which sets `DIO1_FLAG`) is only a **redundant early
//! trigger** — it is consumed if present but is not relied upon.
//!
//! This design is deliberate and hardware-validated on the Heltec WiFi LoRa 32 V3
//! (first TTN OTAA join + uplink, 2026-06-17): `esp-idf-hal` `PinDriver` interrupts
//! are one-shot (auto-disabled on each firing, re-armable only from a non-ISR
//! context), and the DIO1 pin is owned by the `SX126x` driver, so the edge cannot
//! be re-armed from this loop. As a result only the first edge (TxDone) is ever
//! delivered by interrupt; SPI polling catches every subsequent RxDone/Timeout.
//! See `docs/project-lore.md` — "LoRaWAN OTAA Join".
//!
//! Failure mode if this ever regresses: uplinks would TX (the join itself proves
//! the path), but RX1/RX2 downlinks and window-close timeouts would be missed,
//! surfacing as a send loop that stalls to its safety guard. The fix is to keep
//! SPI polling as the primary source — never the DIO1 edge alone.
//!
//! # EU868 duty cycle
//!
//! At SF7/BW125 a 2-byte payload has a ~35 ms airtime, well under the 1% duty-cycle
//! limit on the 868.1 MHz channel (~360 s). 30 s between uplinks is conservative
//! and safe for development use.

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

use rustyfarian_esp_idf_network::lora::{
    config::{LoraConfig, Region},
    sx1262_driver::{EspIdfLoraRadio, LoraRadioAdapter},
};

/// DIO1 rising-edge flag — set by ISR, cleared by main loop after delivery.
static DIO1_FLAG: AtomicBool = AtomicBool::new(false);

/// How often to send an uplink.
const UPLINK_INTERVAL: Duration = Duration::from_secs(30);

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

    let tag = "idf_esp32s3_lora_uplink";

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

    // ─── Internal temperature sensor ─────────────────────────────────────────

    let mut temp_sensor = DieTempSensor::new()?;
    log::info!(target: tag, "Die temperature sensor enabled");

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
                return Err(anyhow::anyhow!("OTAA join failed — no join-accept"));
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
                log::debug!(target: tag, "join response: {:?}", other);
                response = Response::NoUpdate;
            }
        }

        // Safety net — abort after 60 s.
        if elapsed_ms > 60_000 {
            return Err(anyhow::anyhow!("Join timed out after 60 s"));
        }

        std::thread::sleep(Duration::from_millis(10));
    }

    // ─── Uplink loop ──────────────────────────────────────────────────────────

    loop {
        // Read die temperature and encode as big-endian i16 (°C × 100).
        //
        // `round()` before the cast avoids truncation surprises (e.g. 42.499 °C →
        // 4250, not 4249). The sensor is configured for −10..80 °C, so ×100 stays
        // within −1000..8000 — comfortably inside i16; Rust's float→int cast also
        // saturates rather than wrapping, so an out-of-range reading can never
        // produce a wrapped payload.
        let temp_c = temp_sensor.read_celsius();
        let temp_raw = (temp_c * 100.0).round() as i16;
        let payload = temp_raw.to_be_bytes();

        log::info!(
            target: tag,
            "Die temperature: {:.2} °C",
            temp_c,
        );
        log::info!(
            target: tag,
            "Sending uplink: temp={} (0x{:02X}, 0x{:02X})",
            temp_raw,
            payload[0],
            payload[1],
        );

        // Send unconfirmed uplink on fPort 1.
        //
        // Signature: fn send(&mut self, data: &[u8], fport: u8, confirmed: bool)
        //            -> Result<Response, Error<R>>
        let mut send_response = device
            .send(&payload, 1, false)
            .map_err(|e| anyhow::anyhow!("send failed: {:?}", e))?;

        let send_start = Instant::now();
        let mut send_timeout_ms: u32 = 0;

        if let Response::TimeoutRequest(ms) = send_response {
            // Absolute timestamp — assign, don't add elapsed.
            send_timeout_ms = ms;
            log::debug!(target: tag, "TX timeout scheduled at {}ms (absolute)", ms);
            send_response = Response::NoUpdate;
        }

        // Drive the event loop until the send completes.
        let uplink_done = loop {
            let elapsed_ms = send_start.elapsed().as_millis() as u32;

            let dio1_edge = DIO1_FLAG.swap(false, Ordering::AcqRel);
            if dio1_edge || device.get_radio().irq_pending() {
                log::debug!(target: tag, "TX radio IRQ at {}ms", elapsed_ms);
                send_response = device
                    .handle_event(Event::RadioEvent(LdRadioEvent::Phy(())))
                    .map_err(|e| anyhow::anyhow!("{:?}", e))?;
            }

            if elapsed_ms >= send_timeout_ms && send_timeout_ms > 0 {
                let late_ms = elapsed_ms.saturating_sub(send_timeout_ms);
                log::debug!(
                    target: tag,
                    "TX timeout fired at {}ms (deadline {}ms, {}ms late)",
                    elapsed_ms, send_timeout_ms, late_ms
                );
                send_timeout_ms = 0;
                send_response = device
                    .handle_event(Event::TimeoutFired)
                    .map_err(|e| anyhow::anyhow!("{:?}", e))?;
            }

            match &send_response {
                Response::TimeoutRequest(ms) => {
                    // Absolute timestamp — assign, don't add.
                    send_timeout_ms = *ms;
                    log::debug!(target: tag, "TX window at {}ms (absolute)", *ms);
                    send_response = Response::NoUpdate;
                }
                Response::UplinkSending(fcnt) => {
                    log::info!(target: tag, "Uplink on air (FCntUp={})", fcnt);
                    send_response = Response::NoUpdate;
                }
                Response::DownlinkReceived(fcnt) => {
                    // A valid RX1 downlink completes the send cycle: lorawan-device
                    // cancels RX2 and emits no further events, so this is terminal.
                    // (TTN routinely answers the first few uplinks with a MAC-command
                    // downlink such as LinkADRReq — that lands here.)
                    log::info!(target: tag, "Downlink received (FCntDown={})", fcnt);
                    break true;
                }
                Response::RxComplete => {
                    log::info!(target: tag, "Uplink complete (RxComplete)");
                    break true;
                }
                Response::ReadyToSend => {
                    log::info!(target: tag, "Uplink complete (ReadyToSend)");
                    break true;
                }
                Response::NoAck => {
                    // Confirmed uplink timed out waiting for an ACK.  We use
                    // unconfirmed uplinks (confirmed=false), so this should not
                    // fire in normal operation.
                    log::warn!(target: tag, "Uplink NoAck — continuing");
                    break true;
                }
                Response::SessionExpired => {
                    log::error!(target: tag, "Session expired — re-join required");
                    break false;
                }
                Response::NoUpdate => {}
                other => {
                    log::debug!(target: tag, "TX response: {:?}", other);
                    send_response = Response::NoUpdate;
                }
            }

            // Guard against a stuck send loop.
            if elapsed_ms > 30_000 {
                log::error!(target: tag, "Uplink event loop timed out after 30 s");
                break false;
            }

            std::thread::sleep(Duration::from_millis(10));
        };

        if !uplink_done {
            // Session expired or unrecoverable error — stop the loop.
            log::error!(target: tag, "Uplink loop terminated — session expired or timeout");
            return Err(anyhow::anyhow!(
                "uplink failed: session expired or event loop timeout"
            ));
        }

        // Sleep until the next 30 s tick.  The uplink itself takes a few hundred
        // milliseconds (TX + RX windows), so the true period is slightly over 30 s,
        // which is fine for EU868 duty-cycle purposes.
        log::info!(target: tag, "Sleeping for {} s until next uplink", UPLINK_INTERVAL.as_secs());
        std::thread::sleep(UPLINK_INTERVAL);
    }
}

// ─── DieTempSensor — ESP32-S3 internal temperature sensor ────────────────────

/// Wrapper around the ESP-IDF temperature sensor driver (`driver/temperature_sensor.h`).
///
/// The sensor is installed, enabled, and held open for the lifetime of this struct.
/// Readings are available via [`DieTempSensor::read_celsius`].
///
/// # ESP-IDF bindings (IDF v5.3.3)
///
/// `temperature_sensor_config_t` has three fields:
/// - `range_min: c_int` — minimum expected temperature (°C)
/// - `range_max: c_int` — maximum expected temperature (°C)
/// - `clk_src: temperature_sensor_clk_src_t` — clock source; 0 = RC_FAST / DEFAULT
///
/// There is no `flags` field in IDF v5.3.3 bindings.
struct DieTempSensor {
    handle: esp_idf_svc::sys::temperature_sensor_handle_t,
}

impl DieTempSensor {
    /// Install and enable the ESP32-S3 die temperature sensor.
    ///
    /// Configured for the -10 °C to +80 °C range, which covers the expected
    /// operating range of embedded hardware with comfortable margin.
    fn new() -> anyhow::Result<Self> {
        use esp_idf_svc::sys::{
            temperature_sensor_config_t, temperature_sensor_enable, temperature_sensor_handle_t,
            temperature_sensor_install,
        };

        let config = temperature_sensor_config_t {
            range_min: -10,
            range_max: 80,
            // TEMPERATURE_SENSOR_CLK_SRC_DEFAULT == 0 (RC_FAST on ESP32-S3).
            // The Default impl for this struct zero-initialises all fields, so
            // we only set the fields we care about.
            clk_src: 0,
        };

        let mut handle: temperature_sensor_handle_t = core::ptr::null_mut();

        // SAFETY: `config` is a valid, initialised struct on the stack.
        // `handle` is a pointer-to-pointer output parameter; we pass its address.
        // Both pointers are valid for the duration of the call.
        esp_idf_svc::sys::esp!(unsafe { temperature_sensor_install(&config, &mut handle) })
            .map_err(|e| anyhow::anyhow!("temperature_sensor_install failed: {:?}", e))?;

        // SAFETY: `handle` was just initialised by a successful `temperature_sensor_install`.
        esp_idf_svc::sys::esp!(unsafe { temperature_sensor_enable(handle) })
            .map_err(|e| anyhow::anyhow!("temperature_sensor_enable failed: {:?}", e))?;

        Ok(Self { handle })
    }

    /// Read the current die temperature in degrees Celsius.
    ///
    /// Returns the raw sensor value without additional filtering.
    /// On error (sensor not in range, driver fault), logs a warning and returns 0.0.
    fn read_celsius(&mut self) -> f32 {
        let mut out: f32 = 0.0;
        // SAFETY: `self.handle` was validated by `new()` and is alive for the
        // lifetime of this struct. `out` is a valid `f32` on the stack.
        let result = esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::temperature_sensor_get_celsius(self.handle, &mut out)
        });
        match result {
            Ok(()) => out,
            Err(e) => {
                log::warn!("temperature_sensor_get_celsius failed: {:?}", e);
                0.0
            }
        }
    }
}

impl Drop for DieTempSensor {
    fn drop(&mut self) {
        // SAFETY: `self.handle` is valid; disable before uninstall as required
        // by the ESP-IDF driver (enable/disable must be balanced).
        unsafe {
            let _ = esp_idf_svc::sys::temperature_sensor_disable(self.handle);
            let _ = esp_idf_svc::sys::temperature_sensor_uninstall(self.handle);
        }
    }
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
