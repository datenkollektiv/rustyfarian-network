# Runbook: esp-hal Provisioning Extras Hardware Test

Run this after any change to the `rustyfarian-esp-hal-network` provisioning record format, and before the release that ships `TAG_EXTRA`.
It proves the hardware gate of `docs/features/provisioning-boot-identity-v1.md` (a HAL record with extras survives a reboot) and checks the provisioning task stack against the analytic figures in the example doc blocks.

**Reference devices:** an ESP32-C3 (`hal_c3_provision_mqtt`), an ESP32-C6 (`hal_c6_provision_mqtt`) and the CrowPanel S3 (`hal_esp32s3_provision_mqtt`).

## Setup

- One board on USB, or `ESPFLASH_PORT` set; `just doctor` passes; `curl` on the laptop that joins the open SoftAP.
- A flash with no stored record: the provisioned branch halts without starting the portal, so run `just erase-flash` (it asks for confirmation) before each scenario.
- `.env` with `WIFI_SSID` and `MQTT_HOST`; optional `WIFI_PASS`, `MQTT_PORT` (1883), `MQTT_USER`, `MQTT_PASS`, `MQTT_CLIENT_ID`, `OTA_URL`, `DEVICE_NAME` (`hal-extras-test`).
- The portal page has no inputs for extras, so the scenarios submit with `just portal-save`, which fetches the nonce from `GET /` and posts the canonical fields plus `x_*` extras to `POST /save`.
- Its arguments are positional, `just portal-save <host> <extras> <key_len> <value_len>`, defaults `192.168.4.1`, `2`, `6`, `8`; `just portal-save extras=8` would send `extras=8` as the host.
  `key_len` must be at least 5, because shorter keys collide and the portal rejects them as duplicates.
- After a reboot the examples log the device name, the profile and the number of extras on the `Already provisioned` line, then the MQTT port; they do not measure stack usage.

## Scenarios

### H1 — HAL record with extras survives a reboot

Bench: `c6/provision`, `s3/provision`; the C3 runs by hand (C3 hal provisioning extras in the [release runbook](release-0.7.0-hardware-test.md)).

1. `just run hal_c3_provision_mqtt`, wait for `Provisioning AP up — open http://<ip>/ to configure`, join the SoftAP.
2. `just portal-save 192.168.4.1 2` (extras `x_k1aa` and `x_k2aa`); pass: `HTTP 200`, then `Portal: submission accepted`, `Portal: credentials committed to flash` and `Credentials committed`.
3. Unplug and replug USB, `just monitor`; pass: `Already provisioned` with the `DEVICE_NAME`, profile `WifiMqttDevice` and `extras=2`.

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

No check.
The library caps are 8 extras, keys of at most 13 bytes and values of at most 64 bytes.
The example doc blocks give an analytic bound from host `size_of` figures, not a measurement: about 12.5 KiB transient for `ProvisioningStore::save` plus the parsed config, about 20.5 KiB peak, 24 KiB recommended minimum stack.
The portal caps a request at 2048 bytes (security-checklist item 3) and answers `413 Payload Too Large`; 8 maximum extras fit only while the values need no percent-encoding, so browser-encoded values full of `&`, `=` or `%` (three times longer) are rejected rather than stored, while `just portal-save` sends plain ASCII.

1. `just run hal_c3_provision_mqtt`, join the SoftAP, `just portal-save 192.168.4.1 8 13 64`.
2. Pass: `Portal: credentials committed to flash`, with no panic, no stack overflow and no unexpected reset.
3. Power-cycle; pass: `Already provisioned` with the device name and `extras=8`.
4. Repeat on the C6.

The pass result is "no overflow at the default stack", which does not confirm the 20.5 KiB figure; a real peak needs stack painting, which no `just` recipe provides, so record a peak only if you add painting to a local copy of the example.

### H3 — Over-cap input is rejected

No check.

1. `just run hal_c3_provision_mqtt`, join the SoftAP.
2. `just portal-save 192.168.4.1 9`, then `just portal-save 192.168.4.1 2 14`, then `just portal-save 192.168.4.1 2 6 65`.
3. Pass: each prints `HTTP 400`, the serial log shows `POST /save rejected: <n> field error(s)` each time, and no record is written.

### H4 — IDF boot smoke at a small main-task stack

The C3 portal prefill check in the [release runbook](release-0.7.0-hardware-test.md) runs this on `idf_c3_provision_mqtt`.
The IDF `ProvisioningConfig` is boxed for its lifetime, but one by-value copy (about 1.4 KiB) crosses the boot task's stack during `ProvisioningStore::load`.
The workspace `sdkconfig.defaults` sets `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768` and no `just` recipe lowers it, so the workspace example proves only the boot path; the 8000 B check runs in rgb-clock's own repository.
Pass: after the restart `Loaded provisioned config:` then `Device identity: device_name=<your name>, ota_url=set` (or `not set`), no stack overflow, no reset loop.

## Results

| Scenario                                 | Result          | Evidence                                                    |
|:-----------------------------------------|:----------------|:------------------------------------------------------------|
| H1, C3                                   | pass 2026-11-09 | release runbook, C3 hal provisioning extras (manual)        |
| H1, C6                                   | pass 2026-11-10 | `c6/provision`                                              |
| H1, S3                                   | pass 2026-11-11 | `s3/provision`                                              |
| H2, C3 and C6                            | open            |                                                             |
| H2 observed peak stack (needs painting)  | open            |                                                             |
| H3                                       | open            |                                                             |
| H4, workspace example, C3                | pass 2026-11-09 | release runbook, C3 portal prefill (manual)                 |

## Downgrade check (optional)

A record with extras over 512 B fails decode on 0.6.x firmware, which caps its read at 512 B; `pick_active` then falls back to the other sector, so the device quietly boots the previous credentials, or reports unprovisioned if that sector is empty.
To confirm, flash a 0.6.x build of the same example over a board holding an H2 record; pass: previous credentials or unprovisioned, no crash; re-provision afterwards.
