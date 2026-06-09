# ADR 012: ESP-NOW Channel Stability for Unassociated STA

## Status

Accepted

## Context

Hardware testing (2026-06-09) revealed that `send_and_wait` always returns `"peer did not ACK"`
when the scout is an unassociated STA (`init_with_radio`) and the coordinator is connected to a
Wi-Fi AP.
`scan_for_peer` succeeds — the coordinator receives every scan probe — but zero data frames are
delivered.
The coordinator logs confirm it: only `"scout-probe"` frames appear, never `"hello #N"`.

### Root cause

`scan_for_peer` calls `esp_wifi_set_channel(ch, WIFI_SECOND_CHAN_NONE)` immediately before each
unicast probe.
The channel is set to 1, the probe goes out, the coordinator ACKs, the scan returns.
`send_and_wait` then transmits a data frame without resetting the channel.
Between `scan_for_peer` returning and `esp_now_send` executing, the ESP-IDF Wi-Fi driver's
internal state machine (running at higher scheduler priority inside the binary blob) has hopped
the radio to a different channel.
The data frame is transmitted on the wrong channel.
The coordinator (AP-locked to channel 1) never receives it.
Seven 802.11 MAC-layer retry attempts exhaust in ~30 ms; the send callback fires `FAIL`.

The send callback fires `FAIL` (not timeout), confirming the frame was transmitted and retried —
not dropped before TX.
The ~30 ms failure window matches 7 × ~4 ms retry cadence at 2.4 GHz.

This behaviour is caused by the Wi-Fi driver autonomously scanning channels in the disconnected
STA state.
It is **not documented** by Espressif.
Community and Espressif GitHub issues (#4706, #9592, #10341, #12317) confirm it is present in
ESP-IDF v4.4.5+ and worse in v5.1.1+.

### Variant 1 baseline (STA ↔ STA, both AP-connected) is unaffected

A scout that connects to the same AP as the coordinator locks its radio to the AP's channel.
No scanning, no drift, no re-pinning required.
This was verified on hardware (2026-06-09): stable ACKs at 1 s intervals after coordinator
came online, zero unexplained failures.
The channel management problem is specific to the unassociated STA role.

### Approaches evaluated

**Approach A — Promiscuous-bracket re-pin inside `send_and_wait`**

Bracket every channel assertion with `esp_wifi_set_promiscuous(true / false)`.
In STA idle-disconnected mode, the driver's autonomous scanning blocks `esp_wifi_set_channel`
calls that arrive while the scanner is mid-hop.
Promiscuous mode relaxes this lock: `esp_wifi_set_channel` succeeds reliably while promiscuous
is active.
Immediately disabling promiscuous mode after the set leaves the radio on the requested channel
long enough for `esp_now_send` to enqueue and transmit the frame.

This is the dominant community workaround (confirmed across ESP-IDF 3.x, 4.x, 5.x).
It is not documented by Espressif as the canonical solution.
The fix lives entirely inside the library; callers and examples are unchanged.

`EspIdfEspNow` stores the discovered channel from `scan_for_peer` as an `AtomicU8` (sentinel
`u8::MAX` = no channel stored).
`send_and_wait` re-pins only when the driver owns the radio (`_wifi.is_some()`) and a
discovered channel is present.
The AP-connected path (`_wifi.is_none()`, i.e. `init()`) never calls the re-pin — zero impact
on Variant 1.

**Approach A was implemented and validated on hardware (2026-06-10). It was rejected.**

See *Hardware validation — Approach A* below.

*Pros:* No API change. No breaking change. Minimal code delta. Transparent to callers.

*Cons:* As confirmed by hardware testing, the community reports of this trick "working" describe
using it **once at boot** to set a fixed channel before `esp_now_init`, not as a per-send
re-pin over a running session.
Per-send re-pinning fails because `esp_wifi_set_promiscuous(false)` is itself a task-switch
trigger: the Wi-Fi driver task runs at FreeRTOS priority 23; the application task runs at
priority 5.
The driver task preempts the application task between `set_promiscuous(false)` and `esp_now_send`,
hopping the channel before the send executes.
`esp_now_send` then returns `ESP_ERR_ESPNOW_CHAN` (0x3069) because the peer's registered
channel no longer matches the current channel.

**Approach B — SoftAP radio mode in `init_with_radio`**

Start the radio in `WIFI_MODE_AP` instead of `WIFI_MODE_STA`.
An AP must continuously beacon on its configured channel; the driver cannot autonomously hop
channels while beaconing.
This locks the channel durably without any per-send re-pinning.

Requires changing `init_with_radio` to configure a minimal SoftAP and changing
`default_interface()` to return `WifiInterface::Ap`.
All peer registrations for the unassociated role use `ifidx = WIFI_IF_AP`.

ADR 008 (amended) documents that using `WifiInterface::Ap` without an active AP interface
causes `ESP_ERR_ESPNOW_IF`.
Starting in full `WIFI_MODE_AP` resolves that error — the AP interface is active.

`scan_for_peer` channel-hop scans work in AP mode: `esp_wifi_set_channel` in AP mode triggers
a CSA (Channel Switch Announcement); with no associated stations the switch is immediate and
the new channel is stable.

*Pros:* Race-free channel lock — no dependency on scheduler timing or undocumented semantics.
Architecturally sound: an AP role cannot self-preempt to hop channels while it owes beacons.

*Cons:* Breaking API change — `init_with_radio` and `default_interface` semantics change.
Increases RAM usage (AP state, beacon buffer ~500 bytes on ESP32-C3).
Any device also running BLE or another SoftAP would conflict.
Full hardware re-validation needed.

**Approach C — Fixed channel convention, no scan**

Both coordinator and scout agree on a fixed channel at compile-time.
The unassociated scout calls `esp_wifi_set_channel` once at boot and sends directly.

*Pros:* Simplest implementation. No per-send overhead. Eliminates scan latency.

*Cons:* If the coordinator's AP is on a different channel, all frames fail.
Inflexible. Eliminates the channel auto-discovery capability added in ADR 009.

**Approach D — Caller responsibility, document the constraint**

Document that callers must manage the channel themselves.
No library change.

*Pros:* Zero library change. Zero risk.

*Cons:* Bug manifests silently. Every consumer re-discovers the root cause independently.

### Hardware validation — Approach A (2026-06-10)

Three implementations of Approach A were attempted:

**Attempt 1 — bracket before `send`, `send` through Rust wrapper:**

```rust
unsafe { esp_wifi_set_promiscuous(true); esp_wifi_set_channel(ch, ...); esp_wifi_set_promiscuous(false); }
ack_status.reset();          // ← mutex lock here
self.send(mac, data)?;       // ← multiple Rust call frames before esp_now_send
```

Result: `esp_now_send` returned `ESP_ERR_ESPNOW_CHAN` (0x3069) on most calls.
The mutex in `ack_status.reset()` is a FreeRTOS preemption point; the Wi-Fi driver task
preempted before `esp_now_send` executed.
Additionally: the scan produced false positives (e.g. coordinator found on channel 9 when it
was on channel 1) because the background scanner moved the physical channel between
`esp_wifi_set_channel` returning and the probe being transmitted.

**Attempt 2 — bracket spanning send + wait (guard kept active):**

`esp_now_send` cannot operate while promiscuous mode is active.
All sends returned `"failed to send ESP-NOW frame"` immediately.

**Attempt 3 — tightest possible bracket, `esp_now_send` called directly inside `unsafe` block:**

```rust
ack_status.reset();
validate_payload(data)?;
let ret = unsafe {
    esp_wifi_set_promiscuous(true);
    esp_wifi_set_channel(ch, WIFI_SECOND_CHAN_NONE);
    esp_wifi_set_promiscuous(false);
    esp_now_send(mac.as_ptr(), data.as_ptr(), data.len())  // ← immediate next instruction
};
```

Result: success rate improved from ~0.03 % (1/2944 baseline) to ~20 %.
`ic_enable_sniffer` / `ic_disable_sniffer` confirmed the bracket was executing.
Consecutive successes (e.g. "hello #8", "#9", "#10") confirm the mechanism works when the
Wi-Fi driver task happens not to preempt between `set_promiscuous(false)` and the
`esp_now_send` syscall.
`ESP_ERR_ESPNOW_CHAN` still appeared on ~80 % of calls.

**Conclusion:** The tight bracket improves reliability but cannot reach 100 % because
`esp_wifi_set_promiscuous(false)` is itself a task-switch trigger.
The Wi-Fi driver task at priority 23 preempts the application task at priority 5 roughly 80 %
of the time between those two consecutive instructions.
Approach A is fundamentally limited by the FreeRTOS priority inversion between the app task
and the Wi-Fi driver task and is rejected for production use.

## Decision

**Adopt Approach B (SoftAP) as the production default; retain Approach A as a documented
best-effort fallback under a new `init_with_radio_sta` API path.**

Three explicit radio-management modes are exposed instead of two:

| Constructor                   | Mode                          | Channel guarantee                             | Recommended for                                                         |
|:------------------------------|:------------------------------|:----------------------------------------------|:------------------------------------------------------------------------|
| `init` / `init_with_capacity` | `RadioMode::CallerManagedSta` | Caller's responsibility (typically AP-locked) | Devices that share an AP with the peer                                  |
| `init_with_radio`             | `RadioMode::OwnedSoftAp`      | Deterministic via beacon scheduling           | ESP-NOW-only devices (default)                                          |
| `init_with_radio_sta`         | `RadioMode::OwnedStaPromisc`  | Best-effort per-send re-pin (~0–20 % loss)    | Devices where SoftAP conflicts with BLE coexistence or a user-facing AP |

Approach A is rejected as the *primary* solution — its ~80 % failure rate makes it
unsuitable for production — but it is preserved as a fallback because SoftAP is not
universally available (BLE coexistence on the same chip, user-facing AP, etc.).
Consumers that pick the fallback opt into the documented failure characteristic.

### Changes to `rustyfarian-esp-idf-espnow`

**`RadioMode` enum (internal)** — encodes the three states explicitly so that
`default_interface`, `scan_for_peer`, and `send_and_wait` branch on a single field rather
than the implicit pair `(_wifi.is_some(), wifi_interface)`.

**`init_with_radio`** — configure a minimal hidden SoftAP on channel 1 before starting
Wi-Fi.  The AP beacon schedule is enforced inside the Wi-Fi driver; it cannot self-preempt
to hop channels while beacons are due.

```rust
wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
    ssid_hidden: true,
    channel: 1,
    auth_method: AuthMethod::None,
    ..Default::default()
})).context("failed to configure SoftAP for ESP-NOW radio")?;
wifi.start().context("failed to start Wi-Fi SoftAP for ESP-NOW")?;
```

`default_interface()` for this constructor returns `WifiInterface::Ap`.

**`init_with_radio_sta` (new)** — starts the radio in unassociated STA mode and tags the
driver with `RadioMode::OwnedStaPromisc`.  `send_and_wait` brackets every send with a
promiscuous-mode channel re-pin against the channel last reported by `scan_for_peer`.
`default_interface()` returns `WifiInterface::Sta`.

**`EspIdfEspNow` struct** — adds `mode: RadioMode`; `pinned_channel: AtomicU8` is **retained**
because:

1. The `Err` branch of `scan_for_peer` uses it to restore the radio channel and peer
   registration after a failed re-scan in any owned-radio mode, preventing the failure
   cascade where the next `send_and_wait` aborts before TX with "peer not found".
2. The `OwnedStaPromisc` path reads it on every send to drive the promiscuous bracket.

**`default_interface()`** — delegates to `RadioMode::wifi_interface()`.

**`scan_for_peer`** — supported in both owned-radio modes (`OwnedSoftAp` and
`OwnedStaPromisc`); rejected with a clear error in `CallerManagedSta` mode where
channel-hop scanning would break the caller's AP association.

**`send_and_wait`** — branches on `self.mode`:

- `OwnedSoftAp` and `CallerManagedSta`: plain `self.send(...)` — channel is held by the
  beacon schedule or the caller-managed AP association.
- `OwnedStaPromisc`: delegates to a dedicated private `send_with_promisc_repin` helper that
  performs the unsafe promiscuous-on / set-channel / promiscuous-off / `esp_now_send`
  sequence in a single block, with per-step error reporting.

**`init_inner`** — accepts `mode: RadioMode` and stores it.

### What does not change

- `EspNowDriver` trait — no trait changes.
- `scan_for_peer` algorithm — `esp_wifi_set_channel` in AP mode triggers an immediate CSA
  with no connected stations; channel hopping during scan works exactly as in STA mode.
- `idf_c3_espnow_sta_scout` example (Variant 1) — uses `init()`, unaffected.

### Example coverage

Three example variants now ship side-by-side so deployment choice is explicit:

- `idf_c3_espnow_sta_scout` — caller-managed STA, shares an AP with the coordinator.
- `idf_c3_espnow_scout` — `init_with_radio` (SoftAP), recommended default for ESP-NOW only.
- `idf_c3_espnow_scout_promisc` — `init_with_radio_sta` (fallback), with explicit
  connected/scanning state machine so the runtime behaviour of the fallback path is
  legible to readers.

### Hardware validation

Validated on ESP32-C3 (2026-06-10):

1. Scout log shows `"Wi-Fi SoftAP started for ESP-NOW"` on boot.
2. No `ic_enable_sniffer` / `ic_disable_sniffer` noise in the SoftAP send loop.
3. Coordinator receives `"hello #N"` on every cycle — no `"scout-probe"` frames after
   initial discovery.
4. Scout log shows `TX #N — ACK` with zero failures after the first `scan_for_peer`
   succeeds.
5. `idf_c3_espnow_sta_scout` (Variant 1) unaffected.
6. `idf_c3_espnow_scout_promisc` (fallback) confirmed to exhibit the documented
   `ESP_ERR_ESPNOW_CHAN` rate; example recovers automatically by dropping into scanning
   state.

## Consequences

### Positive

- Default channel stability is race-free and requires no per-send workaround.
- Three constructors map cleanly to three deployment patterns; the internal `RadioMode`
  enum makes the differences explicit at every branch.
- AP beacon scheduling is enforced in the Wi-Fi driver binary — no dependency on
  scheduler timing or undocumented flag-check intervals.
- Devices that cannot use SoftAP still have a working (if imperfect) path via
  `init_with_radio_sta`; the documented failure rate is preferable to silent
  rediscovery of the underlying race.

### Negative

- **Breaking semantics change**: `init_with_radio` previously started the radio in
  unassociated STA mode; it now starts SoftAP.  `default_interface()` for this constructor
  now returns `WifiInterface::Ap` instead of `WifiInterface::Sta`.  Downstream code that
  hard-codes `WifiInterface::Sta` on a driver-owned radio must either call
  `default_interface()` or migrate to `init_with_radio_sta` to keep the prior behaviour.
- RAM increases by ~500 bytes on ESP32-C3 in SoftAP mode (AP state and beacon buffer).
- The fallback path retains a residual unsafe block; ADR + rustdoc are the only safeguards
  against a caller misusing it where SoftAP would have worked.

## References

- [ADR 008 — ESP-NOW Radio-Only Initialisation](008-espnow-radio-only-init.md)
- [ADR 009 — ESP-NOW Channel Scanning](009-espnow-channel-scanning.md)
- ESP-IDF issue #4706: `esp_wifi_set_channel` blocked during autonomous scan
- ESP-IDF issue #9592: ESP-NOW example fails on channels other than 1
- ESP-IDF issue #10341: ESP-NOW does not work with WiFi STA mode
- ESP-IDF issue #12317: Behaviour change in disconnected-STA scanning between v4.4.5 and v5.1.1
- `esp-radio` (bare-metal Rust): `EspNow::set_channel()` — dedicated channel API with
  documented "no AP or STA" restriction; confirms the platform-level constraint is real
- `espressif/esp-now` component: `ESPNOW_CHANNEL_ALL` sweep + `CONFIG_ESPNOW_AUTO_RESTORE_CHANNEL`
  — Espressif's own higher-level library works around the same root cause via multi-channel
  broadcast rather than a fixed channel per send
