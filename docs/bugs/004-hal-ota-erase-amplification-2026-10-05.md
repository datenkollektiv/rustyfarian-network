---
id: 004
title: esp-hal OTA download erases each flash sector repeatedly
captured-on: 2026-10-05
doc-version: 2
status: fixed-in-code
kind: defect
---

# Bug 004: esp-hal OTA download erases each flash sector repeatedly

## Symptom
The esp-hal tier's `fetch_and_apply` erases each 4 KiB sector of the target partition about eight times per image instead of once.

## Suspected Cause
`EspHalOtaManager::fetch_and_apply` writes each 512-byte chunk with `FlashRegion::write` (`crates/rustyfarian-esp-hal-network/src/ota/manager.rs`, the `Sink::ReadModifyWrite` arm).
That calls `esp-storage 0.10.0` `FlashStorage::write` (via `esp-bootloader-esp-idf 0.6.0` `partitions.rs:813-829`, whose `FlashAccess::flash_write` forwards to it in `flash_access.rs:42-44`).
`FlashStorage::write` does read-modify-write: read the sector, patch it, erase it, write it back (`esp-storage-0.10.0/src/storage.rs:72-96`).
The problem is repeated erasure, not a missing erase.
Adding an explicit erase before the existing writes would not remove the amplification.

## Linked Artefact
`crates/rustyfarian-esp-hal-network/src/ota/manager.rs`; found while reviewing `docs/features/archive/ota-consumer-runtime-v1.md`.

## Reproduction Confidence
high (from source; no hardware measurement yet)

## Severity
medium

## Environment
esp-hal tier, `esp-storage 0.10.0`, `esp-bootloader-esp-idf 0.6.0`, any chip; not yet observed on hardware.

## Expected Behaviour
Each sector of the target range is erased once per image.

## Actual Behaviour
With full 512-byte chunks each sector is erased about eight times.
Short socket reads produce smaller writes and potentially more erases per sector.
The image is still written correctly; the cost is flash wear and a slower download.

## Reproduction Steps
1. Run an esp-hal tier OTA update (`EspHalOtaManager::fetch_and_apply`) with a 1 MiB image.
2. Instrument or trace `esp-storage` sector erases during the download.
3. Count erases per sector: about 8 instead of 1.

## Suggested Fix Area
Erase the target range once (`FlashRegion::erase`, `esp-bootloader-esp-idf-0.6.0/src/partitions.rs:839`).
Buffer incoming data into sector-aligned blocks.
Write them through the non-RMW `write_nor` path.

## Fix
Implemented 2026-11-02.

- An unencrypted partition is erased once (`FlashRegion::erase` over the image rounded up to whole sectors) and then written through the non-erasing `NorFlash` view (`FlashRegion::as_nor_flash`), so each sector is erased once per image.
- `juggler::ota::ChunkAligner` holds back the remainder of a partial word, so every `write_nor` call sees a 4-byte-aligned offset and length; a socket read of any size fits, and the final partial word is padded with `0xFF` inside the erased span.
  Host-tested, with a mutation check.
- An encrypted partition keeps the read-modify-erase-write path: `as_nor_flash` refuses it, because the ROM encrypts whole sectors.
- The socket read buffer is word-aligned (`#[repr(align(4))]`) so `write_nor` writes from it directly instead of copying through its own 4 KiB sector buffer.
- `esp-bootloader-esp-idf` now enables its `embedded-storage` feature (the `embedded-storage` crate was already a direct dependency).
- Measured effect to confirm on hardware: a 1 MiB image drops from ~2048 erases and ~8 MiB of flash traffic to 256 erases and ~1 MiB.

## Owner

## Links
- `docs/features/archive/ota-consumer-runtime-v1.md` (answers to requester's questions; out of scope for sub-project A)

## Session Log
- 2026-10-05 — Captured as a defect via /bug from the OTA consumer-runtime review
- 2026-11-02 — Challenged against the pinned sources and fixed: the chain was confirmed exactly as filed, and the cost is wider than filed — also ~8x write amplification (~8 MiB of flash traffic per 1 MiB image), tens of seconds of erase time, and a 4 KiB sector buffer on the stack per `write` call.
- 2026-11-02 — Fixed in code: erase once plus the non-erasing NorFlash view behind an alignment planner (`juggler::ota::ChunkAligner`, host tests + mutation check), encrypted partitions unchanged, `esp-bootloader-esp-idf`'s `embedded-storage` feature enabled; `just check-ota-hal-embassy` and `just clippy-hal` green, hardware confirmation pending.
- 2026-11-11 — First run on real flash (`s3/ota`, CrowPanel S3, `hal_esp32s3_ota`, after the OTA TCP connect fix): one `OTA: erasing 663552 bytes of the target partition` line for a 659 472 B image, download, write and SHA-256 verification in 5 s, slot 1 activated and marked valid on the next boot; the unhealthy image was rolled back by the bootloader on the following reset (`previous update was rolled back; not retrying 2.0.0`).
