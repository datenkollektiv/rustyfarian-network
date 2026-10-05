---
id: 004
title: esp-hal OTA download erases each flash sector repeatedly
captured-on: 2026-10-05
doc-version: 1
status: open-defect
kind: defect
---

# Bug 004: esp-hal OTA download erases each flash sector repeatedly

## Symptom
The esp-hal tier's `fetch_and_apply` erases each 4 KiB sector of the target partition about eight times per image instead of once.

## Suspected Cause
`EspHalOtaManager::fetch_and_apply` writes each 512-byte chunk with `FlashRegion::write` (`crates/rustyfarian-esp-hal-network/src/ota/manager.rs` ~206).
That calls `esp-storage 0.10.0` `FlashStorage::write` (via `esp-bootloader-esp-idf 0.6.0` `partitions.rs:813-829`).
`FlashStorage::write` does read-modify-write: read the sector, patch it, erase it, write it back (`esp-storage-0.10.0/src/storage.rs:55-95`).
The problem is repeated erasure, not a missing erase.
Adding an explicit erase before the existing writes would not remove the amplification.

## Linked Artefact
`crates/rustyfarian-esp-hal-network/src/ota/manager.rs`; found while reviewing `docs/features/ota-consumer-runtime-v1.md`.

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

## Owner

## Links
- `docs/features/ota-consumer-runtime-v1.md` (answers to requester's questions; out of scope for sub-project A)

## Session Log
- 2026-10-05 — Captured as a defect via /bug from the OTA consumer-runtime review
