# Bench Report: re-entry hook on a second consumer

| Field   | Value                                                                                  |
|:--------|:---------------------------------------------------------------------------------------|
| From    | rustbox-spookhouse, network spike `firmware/spookhouse-idf-net`                        |
| To      | `rustyfarian-network`, `docs/features/provisioning-reentry-hook-v1.md` (hardware gate) |
| Pin     | `b4f57a007624463acc2d876fcf5cfe226c876612` (`origin/main`)                             |
| Board   | ESP32-C3-DevKitM-1, UART0 console on its own CP2102, SSD1306 status screen             |
| Date    | 2026-10-30                                                                             |
| Status  | Queued 2026-11-11 (copy in network's `review-queue/`)                                  |

## Scope

- This is second-consumer evidence for `WifiMqttBoot::load_with_reentry`, not a run of `idf_c3_provision_mqtt`.
- Spookhouse answers the hook from a stored flag (the pattern in the feature doc's constraints), never from a pin sample.
- Only `ReentryRequest::None` and `ReentryRequest::FactoryReset` are exercised; `Portal` is never answered.
- The consumer erases on `FactoryResetRequested` and opens the portal in the same boot, as agreed in the reply.

## Results

| Case                                   | Outcome                 | Result |
|:---------------------------------------|:------------------------|:-------|
| Cold boot and RST with a stored record | `Ready`                 | Pass   |
| Short BOOT press (< 3 s)               | no request stored       | Pass   |
| BOOT hold 3 s, then restart            | `FactoryResetRequested` | Pass   |
| RST in the portal after the erase      | `NotProvisioned`        | Pass   |
| Portal commit, then restart            | `Ready`                 | Pass   |
| BOOT still held through the restart    | `FactoryResetRequested` | Pass   |
| Deep-sleep GPIO wake and timer wake    | `Ready`                 | Pass   |

## Log Excerpts

```text
10:39:34 portal: reset requested by BOOT hold
10:39:34 portal: reset stored; restarting to erase
10:39:34 rst:0xc (RTC_SW_CPU_RST),boot:0xd (SPI_FAST_FLASH_BOOT)
10:39:35 boot: reset=Software wake=PowerOn resume=Cold slept_ms=-
10:39:35 portal: reset request taken; erasing the settings
10:39:35 provision: settings erased
10:39:35 portal: starting ssid=Spookhouse-C4F5
10:39:35 Provisioning AP up at 192.168.4.1
10:50:51 rst:0x1 (POWERON),boot:0xd (SPI_FAST_FLASH_BOOT)
10:50:52 provision: no settings stored; starting the portal
11:26:01 boot: reset=DeepSleep wake=Timer resume=Restored slept_ms=900709
11:26:01 provision: ready device_name len=10 ... ota_url=none
```

## Findings for Upstream

- The hook path never read the store on a `FactoryReset` answer, and the library erased nothing.
- A software restart (`rst:0xc`) with BOOT still held did not enter download mode on the C3.
- So a consumer that restarts from a held button is safe on the C3; we did not check other chips.
- `WifiMqttBoot::device_name()` and `ota_url()` replaced spookhouse's second store read (`0391e29`).
- The hook adds one NVS read in the consumer's own namespace per boot, including every wake.

## Since the Run

- Spookhouse now pins `bc41ffb5e4db5ca9fddaaeee3b971d352f19b4af` (per-device name pre-fill).
- `b4f57a0..bc41ffb` leaves the hook path alone: in `rustyfarian-esp-idf-network/src` only an import line in `provisioning/mod.rs` changes, so these results carry over.
- `DeviceName::MacSuffixed` changes only the pre-fill a portal shows after the erase, not any outcome above.

## Not Covered

- A record written by `94331ca` firmware loading at `b4f57a0`: the board had nothing stored before the run.
- `ReentryRequest::Portal` and a BOOT hold from inside the fallback portal.
