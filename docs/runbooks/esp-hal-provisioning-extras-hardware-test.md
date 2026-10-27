# Runbook: esp-hal Provisioning Extras Hardware Test

Run this after any change to the `rustyfarian-esp-hal-network` provisioning record format, and before the release that ships `TAG_EXTRA`.
It proves the hardware gate of `docs/features/provisioning-boot-identity-v1.md`: a HAL record with extras survives a reboot on an ESP32-C3 and an ESP32-C6.
It also checks the provisioning task stack against the analytic figures in the example doc blocks.

**Reference devices:** any ESP32-C3 board (`hal_c3_provision_mqtt`) and any ESP32-C6 board (`hal_c6_provision_mqtt`).

## Prerequisites

- The board on USB; `ls /dev/cu.usb*` (macOS) shows exactly one port.
- With two USB serial devices attached, set `ESPFLASH_PORT=/dev/cu.usbmodemXXXX` so `just flash` and `just monitor` pick the right one.
- A phone or laptop that can join the open SoftAP and open its captive portal.
- A flash with no stored record, or a flash you accept to re-provision (`just erase-flash` asks for confirmation).
- The esp toolchain for the chip (`just doctor` passes).
- The dotenv variables `WIFI_SSID` and `MQTT_HOST` are set (required).
- `WIFI_PASS`, `MQTT_PORT` (default 1883), `MQTT_USER`, `MQTT_PASS`, `MQTT_CLIENT_ID`, `OTA_URL` and `DEVICE_NAME` (default `hal-extras-test`) are optional.
- `curl` on the laptop that joins the SoftAP.

The portal page has no inputs for extras, so the scenarios submit the form with `just portal-save`.
The recipe fetches the nonce from `GET /` and posts the canonical fields plus `x_*` extras to `POST /save`.
Its arguments are positional: `just portal-save <host> <extras> <key_len> <value_len>`, with defaults `192.168.4.1`, `2`, `6` and `8`.
`just` has no `name=value` argument syntax, so `just portal-save extras=8` would send the literal `extras=8` as the host.
`key_len` must be at least 5, because shorter keys collide and the portal rejects them as duplicates.

## What the example logs

After a reboot the examples log the stored device name, the profile and the number of stored extras on the `Already provisioned` line, then the MQTT port.
They do not measure stack usage.

## Build, flash, monitor

```bash
just build-example hal_c3_provision_mqtt
just flash hal_c3_provision_mqtt
just monitor
```

For the C6 board replace the example name with `hal_c6_provision_mqtt`.
`just run hal_c3_provision_mqtt` builds, flashes and opens the monitor in one step.

## Scenarios

### H1 — HAL record with extras survives a reboot

1. Optionally run `just erase-flash` so the board starts unprovisioned.
2. Run `just run hal_c3_provision_mqtt` and wait for the line `Provisioning AP up — open http://<ip>/ to configure`.
3. Join the SoftAP from the laptop.
4. Run `just portal-save 192.168.4.1 2`, which submits the dotenv Wi-Fi and MQTT values, the device name and two extras `x_k1aa` and `x_k2aa`.
5. Pass: the recipe prints `HTTP 200` and the serial log shows `Portal: submission accepted`, `Portal: credentials committed to flash` and `Credentials committed`.
6. Power-cycle the board by unplugging and replugging USB, then run `just monitor`.
7. Pass: the log shows `Already provisioned` with the device name from `DEVICE_NAME`, the profile `WifiMqttDevice` and `extras=2`.
8. Repeat on the C6 board with `hal_c6_provision_mqtt`.

<details>
<summary>Expected log (C3, values vary)</summary>

```text
Provisioning AP up — open http://192.168.4.1/ to configure
Portal: submission accepted
Portal: credentials committed to flash
Credentials committed (ssid len=<n>, profile=WifiMqttDevice) — host should reboot to apply
Already provisioned — ssid len=<n>, device_name=<your name>, profile=WifiMqttDevice, extras=2
MQTT target: host len=<n>, port=<port>
```

</details>

### H2 — Extras at their caps, and the stack

The library caps are 8 extras, key at most 13 bytes, value at most 64 bytes.
The example doc blocks state an analytic upper bound from host `size_of` figures: about 12.5 KiB transient for `ProvisioningStore::save` plus the portal's parsed config, and about 20.5 KiB peak, with a recommended minimum stack of 24 KiB.
These figures are not measured, and the examples print no stack figure, so this scenario can only show that the default stack does not overflow.
The portal also caps a whole request at 2048 bytes (security-checklist item 3) and answers a larger one with `413 Payload Too Large`.
A maximum configuration with 8 extras fits that cap only while the values need no percent-encoding; `just portal-save` sends plain ASCII, so H2 stays under it, but values full of characters such as `&`, `=` or `%` expand three times when a browser encodes them and are rejected with 413 rather than stored.

1. Run `just erase-flash`, because the board is still provisioned from H1 and the provisioned branch halts without starting the portal.
2. Run `just run hal_c3_provision_mqtt` and join the SoftAP from the laptop.
3. Run `just portal-save 192.168.4.1 8 13 64`, which posts 8 distinct extras with 13-byte keys and 64-byte values.
4. Watch the serial log through the commit.
5. Pass: the log shows `Portal: credentials committed to flash`, with no panic, no stack-overflow message and no unexpected reset or reboot.
6. Power-cycle and confirm `Already provisioned` with the device name and `extras=8`.
7. Repeat on the C6 board.

Record "no overflow at the default stack" as the pass result.
A real peak needs stack painting, which no `just` recipe provides: if you add it to a local copy of the example, record the observed peak in the results table and compare it with the 20.5 KiB bound.
Without painting, leave the peak cell empty; a pass here does not confirm the 20.5 KiB figure.

### H3 — Over-cap input is rejected

1. Run `just erase-flash`, because the board is provisioned after H2 and the provisioned branch halts without starting the portal.
2. Run `just run hal_c3_provision_mqtt` and join the SoftAP from the laptop.
3. Run `just portal-save 192.168.4.1 9`, then `just portal-save 192.168.4.1 2 14`, then `just portal-save 192.168.4.1 2 6 65`.
4. Pass: each call prints `HTTP 400`, the serial log shows the field-error line `POST /save rejected: <n> field error(s)` for each call, and no record is written.

### H4 — IDF boot smoke at a small main-task stack

The IDF `ProvisioningConfig` is boxed for its lifetime, but one by-value copy (about 1.4 KiB) crosses the boot task's stack during `ProvisioningStore::load`.
rgb-clock runs its main task at 8000 B, so the final review asked for a boot smoke at that stack before the 0.7.0 release.
The workspace `sdkconfig.defaults` sets `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768`, and no `just` recipe lowers it, so this scenario on the workspace example only proves the boot path; the 8000 B check runs in rgb-clock's own build.

1. On an ESP32-C3, run `just run idf_c3_provision_mqtt`, provision it through the portal page in a browser, and let it restart.
2. Pass: after the restart the log shows `Loaded provisioned config:` followed by `Device identity: device_name=<your name>, ota_url=set` (or `ota_url=not set`), with no stack-overflow message and no reset loop.
3. Repeat the same check with rgb-clock's firmware at its 8000 B main-task stack and record the result.

## Results

Fill one row per board.
Leave a cell empty until the scenario has run.

| Scenario                                  | Board | Result      | Evidence (serial line) |
|:------------------------------------------|:------|:------------|:-----------------------|
| H1 two extras survive reboot, C3          |       | pass / fail |                        |
| H1 two extras survive reboot, C6          |       | pass / fail |                        |
| H2 eight extras at caps, no overflow, C3  |       | pass / fail |                        |
| H2 eight extras at caps, no overflow, C6  |       | pass / fail |                        |
| H2 observed peak stack (needs painting)   |       | bytes       |                        |
| H3 over-cap input rejected                |       | pass / fail |                        |
| H4 IDF boot smoke, workspace example, C3  |       | pass / fail |                        |
| H4 IDF boot smoke, rgb-clock at 8000 B    |       | pass / fail |                        |

## Downgrade check (optional)

A record with extras that exceeds 512 B fails decode on 0.6.x firmware, which caps its read at 512 B.
`pick_active` then falls back to the other sector, so the device quietly boots the previous credentials, or reports unprovisioned if the other sector is empty.
To confirm, flash a 0.6.x build of the same example over a board holding a record from H2.
Pass: the device boots the previous credentials or reports unprovisioned, and does not crash.
Re-provision afterwards.
