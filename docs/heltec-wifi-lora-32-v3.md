# Heltec WiFi LoRa 32 V3 — Hardware Reference

Board reference for bringing up the SX1262 radio on the Heltec WiFi LoRa 32 V3
in a bare-metal (`no_std`) Rust environment using `esp-hal 1.0`.
Covers pin assignments, the TCXO power requirement, correct initialisation order, and SPI API usage.

*Research conducted: March 2026.*
*Schematic version: HTIT-WB32LA V3.1 / V3.2.*

---

## Board overview

The Heltec WiFi LoRa 32 V3 (internal name HTIT-WB32LA) is an ESP32-S3 + SX1262 development board.
It integrates Wi-Fi, BLE, LoRa, a 0.96" OLED display (SSD1306), and a LiPo battery management circuit.

Notable differences from V2:

- LoRa chip changed from SX1276 to **SX1262**.
- Crystal oscillator replaced with a **TCXO** (temperature-compensated crystal oscillator) for improved frequency stability.
- MCU changed from ESP32 (Xtensa LX6) to **ESP32-S3** (Xtensa LX7, 240 MHz dual-core).

---

## SX1262 pin assignments

These GPIO numbers are confirmed across the official Heltec schematic (V3.1/V3.2),
the ESPHome device page, and the `ropg/heltec_esp32_lora_v3` RadioLib library.

| Signal | ESP32-S3 GPIO | Direction | Notes |
|:-------|:-------------:|:---------:|:------|
| NSS / CS | **8** | output | Active-low chip select |
| SCK | **9** | output | SPI clock |
| MOSI | **10** | output | |
| MISO | **11** | input | |
| RESET | **12** | output | Active-low; open-drain preferred |
| BUSY | **13** | input | High = chip busy; poll before every command |
| DIO1 (IRQ) | **14** | input | Join-accept and TX-done interrupt |

### DIO1 naming confusion

Heltec labels GPIO 14 as "DIO0" in their `pins_arduino.h` and in some of their own software.
The SX1262 has no DIO0 — its first interrupt pin is DIO1.
GPIO 14 is correctly wired to the SX1262's DIO1 pad; the Heltec label is simply wrong.
Use the name **DIO1** when reading the SX1262 datasheet and configuring IRQ masks.

---

## OLED display pins

| Signal | GPIO |
|:-------|:----:|
| SDA (I2C) | **17** |
| SCL (I2C) | **18** |
| RESET | **21** |

---

## Critical: TCXO powered via DIO3

The Heltec V3 routes the TCXO power supply through the SX1262's DIO3 pin rather than a fixed rail.
This means **the radio has no clock until the host explicitly enables DIO3 as a voltage output**.

Without calling `SetDIO3AsTCXOCtrl` as the first SPI command after reset,
all subsequent commands will stall or return garbage — the chip has no reference clock to operate from.

TCXO parameters confirmed by the official ESPHome device configuration for this board:

| Parameter | Value |
|:----------|:------|
| Supply voltage | **1.8 V** (register code `0x02`) |
| Startup delay | **5 ms** (= 320 × 15.625 µs ticks = `0x000140`) |

`SetDio2AsRfSwitchCtrl (0x9D)` should also be sent immediately after.
DIO2 controls the onboard RF switch; enabling this command lets the SX1262 assert the antenna
switch automatically at the correct moment during TX, without any CPU involvement.

---

## Correct initialisation sequence

<details>
<summary><strong>Step-by-step with SPI bytes</strong></summary>

All SPI transactions: NSS low → write command bytes → optionally read → NSS high.
Poll BUSY low before every transaction.

**Step 1 — Hardware reset**

Assert RESET (GPIO 12) low for at least 100 µs, then release high.
Wait for BUSY (GPIO 13) to go low — the chip signals readiness within ~3 ms.

**Step 2 — Enable TCXO via DIO3 (mandatory on Heltec V3)**

Command `SetDIO3AsTCXOCtrl (0x97)`:

```
write: [0x97, 0x02, 0x00, 0x01, 0x40]
         ^     ^     ^-----------^
         cmd   1.8V  5 ms timeout (3 bytes big-endian)
```

Wait BUSY low.

**Step 3 — Enable RF switch via DIO2 (recommended)**

Command `SetDio2AsRfSwitchCtrl (0x9D)`:

```
write: [0x9D, 0x01]
         ^     ^
         cmd   enable
```

Wait BUSY low.

**Step 4 — Verify chip with GetStatus**

Command `GetStatus (0xC0)`:

```
write: [0xC0, 0x00]   (second byte is a NOP)
read:  [_,    status]
```

The first read byte is undefined; the second byte is the status.
Expected value after reset and TCXO init: **`0x22`**.

Status byte layout:

| Bits | Field | Value after reset | Meaning |
|:-----|:------|:-----------------:|:--------|
| 6:4 | Chip mode | `0x2` | STDBY_RC — correct post-reset mode |
| 3:1 | Command status | `0x1` | idle — normal after a configuration command |

The datasheet labels cmd_status `0x1` as "reserved", but in practice it is the normal response
after a write-only configuration command such as `SetDIO3AsTCXOCtrl` or `SetDio2AsRfSwitchCtrl`
(these commands do not return a data payload, so there is nothing "available to host").
`cmd_status=0x2` ("data available") only appears after commands that explicitly return data.
The authoritative success indicator is `chip_mode=STDBY_RC (0x2)` — not `cmd_status`.

If BUSY never goes low or `GetStatus` returns `0x00`, the TCXO was not initialised correctly.

</details>

---

## SPI API — `esp-hal 1.0` + `embedded-hal-bus`

`esp-hal 1.0` implements the `SpiBus` trait, not `SpiDevice`.
The `SpiDevice` abstraction (which handles CS pin assertion per transaction) must be layered on top
using `ExclusiveDevice` from the `embedded-hal-bus` crate.

### Cargo dependencies

```toml
embedded-hal     = "1.0"
embedded-hal-bus = { version = "0.2", default-features = false }
```

The `no_std` `ExclusiveDevice::new_no_delay` path requires no atomics or `critical-section`;
`default-features = false` is sufficient.

### Wiring pattern

```rust
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::gpio::{Level, Output, OutputConfig, Input, InputConfig, Pull};
use esp_hal::time::RateExtU32;
use embedded_hal::spi::Operation;
use embedded_hal_bus::spi::ExclusiveDevice;

let spi_bus = Spi::new(
    peripherals.SPI2,
    SpiConfig::default()
        .with_frequency(1_u32.MHz())  // start low for bring-up; SX1262 max is 16 MHz
        .with_mode(Mode::_0),
)
.unwrap()
.with_sck(peripherals.GPIO9)
.with_mosi(peripherals.GPIO10)
.with_miso(peripherals.GPIO11);

let cs   = Output::new(peripherals.GPIO8,  Level::High, OutputConfig::default());
let rst  = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());
let busy = Input::new(peripherals.GPIO13, InputConfig::default().with_pull(Pull::None));
let dio1 = Input::new(peripherals.GPIO14, InputConfig::default().with_pull(Pull::None));

let mut spi = ExclusiveDevice::new_no_delay(spi_bus, cs).unwrap();
```

Use `spi.transaction(&mut [Operation::Write(&[...]), Operation::Read(&mut [...])])` for each command.
The `ExclusiveDevice` wrapper asserts NSS at the start of the transaction and releases it at the end.

---

## Common bring-up failure modes

| Symptom | Likely cause |
|:--------|:-------------|
| BUSY never goes low after reset | TCXO not initialised; `SetDIO3AsTCXOCtrl` was not sent |
| `GetStatus` returns `0x00` or garbage | TCXO not initialised; radio has no clock |
| SPI transaction stalls | BUSY not polled before command; chip still processing previous command |
| All commands time out | SPI mode mismatch (SX1262 requires Mode 0: CPOL=0, CPHA=0) |
| No IRQ on join-accept | DIO1 confused with Heltec's "DIO0" label; verify GPIO 14 |
| RF switch not toggling | `SetDio2AsRfSwitchCtrl` not called; TX power appears correct but no signal |

---

## Sources

- [WiFi LoRa 32 V3 — Heltec official product page](https://heltec.org/project/wifi-lora-32-v3/)
- [HTIT-WB32LA V3.1 Schematic (PDF)](https://resource.heltec.cn/download/WiFi_LoRa_32_V3/HTIT-WB32LA(F)_V3.1_Schematic_Diagram.pdf)
- [HTIT-WB32LA V3.2 Datasheet (PDF)](https://resource.heltec.cn/download/WiFi_LoRa_32_V3/HTIT-WB32LA_V3.2.pdf)
- [GPIO Usage Guide — Heltec Wiki](https://wiki.heltec.org/docs/devices/open-source-hardware/esp32-series/lora-32/wifi-lora-32-v3/Pin-diagram-guidance)
- [Heltec WiFi LoRa 32 V3 — ESPHome device page](https://devices.esphome.io/devices/heltec-wifi-lora-32-v3/) (confirms TCXO voltage 1.8 V, delay 5 ms)
- [ropg/heltec_esp32_lora_v3 — RadioLib Arduino library](https://github.com/ropg/heltec_esp32_lora_v3) (community-verified pins)
- [Semtech SX1261/2 Datasheet Rev 2.x](https://m5stack-doc.oss-cn-shenzhen.aliyuncs.com/1177/DS_SX1261_2_V2-2.pdf)
- [Semtech SX1261/2 Datasheet Rev 1.2 — Sparkfun mirror](https://cdn.sparkfun.com/assets/6/b/5/1/4/SX1262_datasheet.pdf)
- [ExclusiveDevice — embedded-hal-bus docs](https://docs.rs/embedded-hal-bus/latest/embedded_hal_bus/spi/struct.ExclusiveDevice.html)
- [How to use Exclusive SPI Bus — esp-hal issue #1950](https://github.com/esp-rs/esp-hal/issues/1950)
