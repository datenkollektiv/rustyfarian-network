# Reply: hardware re-entry trigger for `wifi-portal-fallback-v1`

| Field   | Value                                                                                         |
|:--------|:----------------------------------------------------------------------------------------------|
| From    | rustbox-spookhouse, network spike `firmware/spookhouse-idf-net`                               |
| To      | `rustyfarian-network`, crate `rustyfarian-esp-idf-network` (`provisioning`)                   |
| Answers | Decision proposal "hardware re-entry trigger" (2026-10-27) and the open questions in `docs/features/provisioning-reentry-hook-v1.md` |
| Date    | 2026-10-27, revised 2026-10-28                                                                |
| Status  | Ready to hand upstream                                                                        |

Read against spookhouse at `cff2a7b` plus working tree (network pinned at `94331ca`) and upstream `82fe395` on the local branch `re-entrtant-hook`, not yet on `origin/main`.
The 2026-10-27 draft of this reply predates `82fe395`; its counter-proposal assumed the library erases and is withdrawn.

## Summary

We accept the hook as implemented in `82fe395`: `load_with_reentry`, `FnOnce() -> ReentryRequest`, the consumer erases, two new outcome variants.
No API change is requested.
Spookhouse will answer the hook from a stored flag, not from a pin sample; the closure shape already allows that.

## How spookhouse will use the hook

A boot-time pin sample does not fit spookhouse:

- The BOOT hold (GPIO9, 3 s) is a runtime gesture: `input.rs` polls it on its own thread for the whole uptime.
- GPIO9 low at reset selects ROM download mode, so the hold cannot span the reset.
- The input thread owns the GPIO9 driver and starts before `provision::load`, so the hook cannot read the pin without a hand-over.
- A deep-sleep node boots every 15 min on the timer; any wait in the hook costs every one of those boots.

So the flow becomes:

1. A full BOOT hold at runtime stores a re-entry flag in spookhouse's own NVS namespace (`sh_net`) and restarts.
2. On the next boot the hook reads and clears the flag and answers `FactoryReset`, otherwise `None`.
3. On `FactoryResetRequested` spookhouse erases `rf_prov` and opens the first-boot portal in the same boot.

The flag is cleared before the hook returns, so a failed erase cannot loop.
Please mention this "runtime gesture → stored flag → hook" pattern in the feature doc next to the pin-sampling example; no code change.

## Answers to the open questions

1. **`Portal` or `FactoryReset`?** `FactoryReset` is our only gesture today.
   Keep `Portal`: spookhouse already reopens a record-keeping portal after 5 credential strikes and may route that through the hook later.
2. **One boot continuing into the portal?** Acceptable, and preferred over a second restart.
   The portal runs on spookhouse's main task with no network thread, and a node without a network thread never deep-sleeps (`NetLink::Absent` in `spookhouse-net-core/src/sleep.rs`).
3. **Should `FactoryReset` wait for a pending OTA image?** No.
   Waiting would silently ignore an explicit request; a later rollback boots the old image into an erased store and the portal, which is acceptable.
   Since `82fe395` the consumer erases, so the library has nothing to wait for; it only must never mark the running image valid on the hook path.
   Spookhouse has no OTA yet (step 7 deferred, `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y` only).
4. **esp-hal tier hook?** No demand from spookhouse: its esp-hal prop has no network.
5. **Timeout or PSK on a fallback portal** (proposal question 3, not carried into the feature doc): leave both to the consumer.
   Spookhouse uses 10 min for the fallback portal and none at first boot; the PSK stays optional (decided 2026-10-26).
   We read `load_prefill`: secrets are never pre-filled, which is what we needed confirmed.

## Context for the counter (rest of `wifi-portal-fallback-v1`)

Spookhouse benched its own failed-join counter in step 6 (2026-10-27):

- Failed joins are classified and counted in NVS namespace `sh_net`; the limit is `CREDENTIAL_STRIKE_LIMIT` = 5.
- At the limit the node restarts into a fallback portal that keeps the stored record and pre-fills from it.
- A BOOT erase or a portal commit clears the count; opening the fallback portal clears it too, so a timeout retries afresh.

If a library counter lands, spookhouse migrates to it and drops its own.

## What spookhouse needs to adopt

- `82fe395` (and `0391e29` below it) on `origin/main`: spookhouse pins only pushed `main` commits.
- The hardware gate in `provisioning-reentry-hook-v1.md` passed, or a note that it is open.

## Acceptance (unchanged from our request)

- The doc names spookhouse as a second consumer and records the hardware-trigger decision.
- A host test or bench run shows that after a fallback portal times out with no commit, `ProvisioningStore::load` returns the record it held before.
- A counter-driven fallback path has no call to `erase_all`, checkable in review.
