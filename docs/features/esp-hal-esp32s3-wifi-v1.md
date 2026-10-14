---
gate: none
desk-work: available
---

# Feature: ESP32-S3 Wi-Fi, provisioning and OTA on the esp-hal tier v1

Upstream request U2 from the `rustyfarian-knob` esp-hal parity quest (`review-queue/feature-request-esp-hal-network-esp32s3-wifi.md`).

## Decisions

| Decision                                                 | Reason                                                        | Rejected Alternative           |
|:---------------------------------------------------------|:--------------------------------------------------------------|:-------------------------------|
| Extend `esp32s3` with radio/rtos/alloc/storage/backtrace | Every pinned crate ships an `esp32s3` feature                 | Keep S3 LoRa-only              |
| Add `esp32s3` to every C3/C6 `any(...)` cfg arm          | Wi-Fi/provisioning code is chip-agnostic above esp-radio      | Per-chip modules               |
| OTA S3 arms mirror the existing `esp32` Xtensa arms      | Same core family, same partition handling                     | New S3-specific code path      |
| `TARGET_CHIP = "esp32s3"`                                | Manifest target check needs a per-chip id                     | Reuse `esp32` id               |
| Example `hal_esp32s3_connect_async`                      | Minimum hardware proof; scripts already route `hal_esp32s3_*` | Validate via provisioning only |
| MQTT out of scope                                        | `esp-hal-mqtt-v1` gates its own chip arms to include S3       | Bundle S3 into MQTT work       |

## Constraints

- Pinned esp-hal wave stays as is (`esp-hal =1.2.2`, `esp-radio =1.0.0-beta.1`).
- C3, C6, ESP32 and S3 LoRa builds must stay unchanged.
- Xtensa flashing needs the IDF-built bootloader workaround from `docs/project-lore.md`.
- Wi-Fi credentials come from `.env` only, never from git.

## Open Questions

- [x] 72 KiB internal heap is enough (~26 KB free after lease); esp-radio allocates internal-only, so PSRAM cannot help Wi-Fi.
- [ ] Does an S3 OTA slot swap work? Needs an OTA partition table and image server; deferred.
- [ ] Optional `hal_esp32s3_provision_mqtt` example not written.

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated
- [x] Hardware validated (STA join + DHCP)

## Session Log

- 2026-10-13 — Feature doc created via /feature from upstream request U2
- 2026-10-14 — Implemented; clippy-hal/verify green; CrowPanel STA join + DHCP validated
