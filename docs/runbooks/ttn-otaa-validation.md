# TTN v3 EU868 OTAA validation

Moved from docs/ROADMAP.md on 2026-11-11; LoRa hardware validation is deferred from release 0.7.0, and no bench check covers it.

<details>
<summary><strong>Validation checklist</strong></summary>

Goal: end-to-end OTAA join, first uplink and first downlink with the least moving parts, all on TTN v3 EU868.

**Step 0 — Credentials**

- Create a TTN application and register an end device (LoRaWAN MAC V1.0.3, RP001-1.0.3, OTAA).
- Record DevEUI (8 bytes), JoinEUI/AppEUI (8 bytes), AppKey (16 bytes).
- Decide byte order: TTN displays EUIs as big-endian strings, many stacks expect LSB-first in memory.
  Log DevEUI/JoinEUI as bytes and compare against the `lorawan-device` documentation before flashing; see `docs/project-lore.md` — "EUI byte order".

**Step 1 — Gateway & RF sanity**

- Confirm a TTN-connected EU868 gateway is online (TTN Console → Gateways → "connected recently").
  For the Pi 4 + RAK5146 SPI EU868/GPS gateway, see the [operational runbook](https://devops.datenkollektiv.de/pages/lorawan-gateway-rak5146-trixie.html) and the shorter [Trixie gateway write-up](https://devops.datenkollektiv.de/lorawan-gateway-rak5146-trixie.html).
- Place the device within metres for initial tests, with a correct EU868 antenna.

**Step 2 — SX1262 bring-up (before LoRaWAN)**

- Verify SPI mode 0 at 8 MHz; confirm the NSS/CS, BUSY, RESET and DIO1 pins.
- Issue a status command after reset and log the response.
- Confirm BUSY goes high during operations and returns low; if BUSY is never handled, every SPI command stalls — see `docs/project-lore.md` — "BUSY pin".

**Step 3 — TTN Live Data setup**

- Open TTN Console → Application → End Device → Live Data and leave it open during testing.
- Enable join-accept and uplink viewing; confirm gateway metadata (RSSI/SNR) is visible.

**Step 4 — OTAA join**

- Firmware logs "joining..." and then either "joined" or the failure reason.
- Live Data shows join-request uplink(s), then the join-accept downlink.
- Join-request visible but no join-accept: wrong AppKey or EUI byte order.
- Join-accept visible in TTN but the device never joins: RX timing or DIO1 IRQ (see `docs/project-lore.md` — "DIO1 interrupt" and "RX window").
- Tune `RX_WINDOW_OFFSET_MS` if windows are missed; start at -200 ms and adjust upward.

**Step 5 — First uplink**

- After the join, send a small payload (1-8 bytes) on FPort 1.
- TTN Live Data shows the uplink with the payload bytes and RSSI/SNR.
- Do not send before the join completes; `LorawanDevice::send()` guards this, and the guard must hold once the real state machine is wired.

**Step 6 — First downlink (port 10 OTA commands)**

- In TTN Console schedule a downlink: FPort 10, payload `01` (CheckUpdate).
- Trigger an uplink first; downlinks only arrive in the RX windows after an uplink.
- Confirm `parse_ota_command()` receives the payload.
- Test `05` (ReportVersion) and `02 01 02 03` (UpdateAvailable 1.2.3).

**Step 7 — Deep sleep / session persistence**

- After the join, persist `LorawanSessionData` (implement the CRC-32 check before relying on restore).
- Sleep and wake; confirm TTN accepts later uplinks with an incremented `FCntUp`.
- A reset or reused `FCntUp` makes TTN reject the frames silently — see `docs/project-lore.md` — "Frame counter reuse".

**Common pitfalls quick reference**

| Symptom                                    | Likely cause                                           |
|:-------------------------------------------|:-------------------------------------------------------|
| Join-request visible, no join-accept       | Wrong AppKey or EUI byte order mismatch                |
| Join-accept in TTN, device stays `Joining` | RX window timing off, or DIO1 IRQ not delivered        |
| All SPI commands stall / timeout           | BUSY pin not polled before each command                |
| Downlinks queued but never received        | No uplink to open the RX window; or wrong FPort        |
| Post-sleep uplinks rejected by TTN         | `FCntUp` reset to 0 (session key/counter not restored) |
| Never joins but gateway is nearby          | Wrong frequency plan (US915 vs EU868) or no antenna    |

</details>
