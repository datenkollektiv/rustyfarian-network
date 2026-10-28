# Decision proposal: hardware re-entry trigger for `wifi-portal-fallback-v1`

| Field   | Value                                                                  |
|:--------|:-----------------------------------------------------------------------|
| From    | `rustyfarian-network`                                                  |
| To      | `rustbox-spookhouse`, network spike `firmware/spookhouse-idf-net`      |
| Answers | Request "second consumer for `wifi-portal-fallback-v1`" (2026-10-26)   |
| Date    | 2026-10-27                                                             |
| Status  | Proposed; needs spookhouse feedback before the design is approved      |

## Proposal1q

`rustyfarian-esp-idf-network` adds an optional re-entry hook to the `WifiMqttDevice` boot path.
The consumer answers it once, after boot and before `WifiMqttBoot::load` reads the store.
The library owns the ordering and the erase; the consumer owns the pin, the hold time and the UI.

## Split of responsibilities

| Concern                                   | Owner       |
|:------------------------------------------|:------------|
| Which GPIO, active level, `BOOT_HOLD_MS`  | Consumer    |
| Sampling after boot, never at reset       | Consumer    |
| Asking the hook before the store is read  | Library     |
| Erasing `rf_prov` on an explicit request  | Library     |
| Opening the portal, reporting the outcome | Library     |
| Log line and OLED state per outcome       | Consumer    |

## Shape (illustrative, not final)

The hook returns one of three answers, and the library runs it at most once per boot.

```rust
pub enum ReentryRequest {
    None,
    Portal,
    FactoryReset,
}
```

- `None`: normal boot; the counter-driven fallback applies as usual.
- `Portal`: open the portal now, keep the stored `ProvisioningConfig` and prefill from it.
- `FactoryReset`: erase the provisioning namespace, then continue as an unprovisioned device.

The hook reaches the boot path through `BootConfig` or a new policy struct, as a closure the consumer supplies.
New `WifiMqttLoadOutcome` variants report what happened, so spookhouse can show a distinct OLED state:

- `ReentryPortal` after a `Portal` or `FactoryReset` answer;
- `FallbackPortal { reason, failures }` after the failed-join counter reaches its threshold.

`WifiMqttLoadOutcome` is `#[non_exhaustive]`, so adding variants does not break existing callers.
Since 9604a9d (2026-10-27) `WifiMqttBoot::load` hands out juggler's `ProvisioningConfig`; the prefill reads that type.

## Rules that hold either way

- Only an explicit request erases: a `FactoryReset` answer or the portal's factory-reset button.
- The counter-driven fallback never calls `erase_all` and keeps the stored record and the prefill.
- A fallback portal that times out without a commit leaves `ProvisioningStore::load` returning the record it held before.
- The hook cannot fail the boot; a load failure keeps the node running without network.
- An answer of `Portal` or `FactoryReset` resets the failed-join counter.

## For spookhouse

Your `input.rs` keeps the BOOT (GPIO9) sampling, `spookhouse-net-core` keeps `BOOT_HOLD_MS` = 3 s, and the hook answers the hook with `FactoryReset` on a full hold.
You no longer erase `rf_prov` or restart yourself; the same boot continues into the portal.
Sample without blocking when BOOT is released, so a normal boot pays no 3 s delay.

## Why a hook, not consumer-owned

- The library guarantees the trigger is read before the store, which consumer code cannot enforce.
- The trigger and the counter share one entry point and one set of outcome variants.
- Board details stay in the consumer, so the library learns no pin numbers or strapping rules.

Rejected: consumer-owned GPIO, erase and restart.
It needs no new API, but every consumer rewrites the same sampling-then-erase sequence and can get the order wrong.

## Questions back to spookhouse

1. Do you need `Portal` (keep the record), or is `FactoryReset` your only answer?
2. Is one boot continuing into the portal acceptable, or do you rely on the restart?
3. With `portal_timeout: None` and an open AP unless `PROVISION_AP_PSK` is set, should a `FallbackPortal` force a timeout or a PSK on a deployed node?
4. If an OTA image is pending verification, should a `FactoryReset` answer wait until that image is confirmed or rolled back?
