# ADR 022: Tier Parity — One Async API Surface on Both Tiers

## Status

Proposed — 2026-11-01; the direction (async API on both tiers) is decided by the maintainer, and the details are to be accepted after review.

## Context

Request U2g from `rustyfarian-knob` triggered this ADR: the knob runs one application on both tiers and writes its own adapter for each tier today (upstream request).
`juggler` already shares the decisions: `LinkMachine`, `LinkState`, back-off, `QoS`, `MqttConfig`, `TryPublishError<E>`, and the provisioning and OTA cores.
No consumer code compiles against both tiers in any domain today.
The root cause is that `rustyfarian-esp-idf-network` is blocking (std threads) while `rustyfarian-esp-hal-network` is async (embassy).
No rule says how names and types must match across the tiers, so every feature drifted, U2f included (see [wifi-reconnect-and-suspend-v1](../features/wifi-reconnect-and-suspend-v1.md)).

Two shared items are stale:

- `juggler::wifi::WifiDriver` is re-exported by both tiers, but only the mock implements it, while its docs name `WiFiManager` as the implementor.
- `juggler::mqtt::MqttConnectionState` documents ESP-IDF behaviour, but the IDF tier never returns it.

The API gaps at `c391a8a` are:

| Area                 | ESP-IDF (`rustyfarian-esp-idf-network`)       | esp-hal (`rustyfarian-esp-hal-network`)           |
|:---------------------|:----------------------------------------------|:--------------------------------------------------|
| Wi-Fi owner          | `WiFiManager`                                 | `AsyncWifiHandle`, `static WifiLink`, `link_task` |
| Wi-Fi config         | `IdfWifiConfig`, `WiFiConfigExt`              | `HalWifiConfig`                                   |
| Link state getter    | `link_state()`                                | `state()`                                         |
| Wi-Fi `is_connected` | `anyhow::Result<bool>`                        | `bool`                                            |
| Suspend and resume   | `anyhow::Result<()>`, synchronous             | `()`, a signal                                    |
| Suspended query      | missing                                       | `is_suspended()`                                  |
| Connect count        | missing (Wi-Fi)                               | `connect_count()`                                 |
| Wait for IP          | `wait_connected(timeout) -> Result<Ipv4Addr>` | `wait_for_ip().await`                             |
| SoftAP start         | `SoftApManager::start`                        | `init_softap_async`                               |
| MQTT payload         | `&str`                                        | `&[u8]`                                           |
| MQTT state           | missing                                       | `state()`                                         |
| Acknowledged publish | `publish_acked` (blocking)                    | `publish_with` (async)                            |
| Startup message      | no-arg flag                                   | `(topic, payload, qos, retain)`                   |
| Provisioning AP IP   | `std::net::Ipv4Addr`                          | `[u8; 4]`                                         |
| Provisioning commit  | `wait_committed(timeout) -> Option`           | `wait_committed().await -> Result`                |
| Provisioning events  | `on_event` closure                            | `fn` pointer                                      |
| OTA                  | `OtaSession`                                  | `EspHalOtaManager`                                |
| ESP-NOW init         | `init_*` constructors                         | `from_controller`, `new_owned`                    |
| ESP-NOW send         | `send_and_wait(mac, data, timeout_ms)`        | `send_and_wait(mac, data)`, async sends           |

The behaviour gaps at `c391a8a` are:

| Behaviour                    | Difference                                                                | Treatment                             |
|:-----------------------------|:--------------------------------------------------------------------------|:--------------------------------------|
| Suspend                      | IDF stops the radio, esp-hal only disconnects (no stop in esp-radio)      | Capability, documented                |
| MQTT reconnect after link-up | IDF waits for the esp-mqtt retry (up to 10 s), esp-hal reconnects at once | Feature: IDF reconnect on link-up     |
| MQTT broker address          | esp-hal needs an IPv4 literal, no DNS (ADR 019)                           | Feature: DNS on esp-hal               |
| MQTT subscriptions           | esp-hal accepts subscriptions only at build time                          | Feature: runtime subscribe on esp-hal |
| Inbound messages             | IDF delivers through callbacks, esp-hal through a `receive()` channel     | Trait: async `receive` (decision 4)   |
| OTA over MQTT                | no OTA runtime on esp-hal                                                 | Feature: OTA runtime on esp-hal       |
| Provisioning boot helpers    | `load_with_reentry` and `run_wifi_mqtt_portal` are IDF-only               | Feature: provisioning boot on esp-hal |
| LoRa                         | esp-hal LoRa is a stub                                                    | Feature: esp-hal LoRa                 |

Feasibility rests on `esp-idf-svc` 0.53 (pinned), which ships `AsyncWifi` (`start`, `connect`, `wait_netif_up`), `EspAsyncMqttClient` and `EspAsyncMqttConnection` (`publish`, `subscribe`, `next`), `EspAsyncTimer`, and an `embassy-time-driver` feature.
That feature lets embassy-time work on both tiers.

## Decision

1. Tier parity is the primary API goal: consumer application code is exchangeable between tiers, and only the dependency and the setup code differ.
2. Async is the primary API on both tiers.
   - The ESP-IDF tier gains its async surface as async wrappers over its existing worker threads (amended 2026-11-01 after the tier-parity Step 2 spike on an ESP32-C3).
   - The spike measured equal start-up, message load and outage recovery for both foundations; the native `esp-idf-svc` async types saved 15 to 25 KB of heap only by leaving out the library runtime.
   - A port on the native types would have to rebuild reconnect, suspend, session confirmation, non-blocking publish and the ADR 017 guard, so the heap gap is closed in the library instead (bug 013).
   - The firmware owns the executor, following the ADR 021 rule that libraries never select a provider.
3. The existing blocking ESP-IDF API stays during 0.x as an additive change.
   - New features land async-first on both tiers.
   - Deprecating the blocking surface is a later, separate decision.
4. Shared traits in `juggler` define the surface, with an associated `Error` type per tier.
   - The `Error` type is `no_std` on esp-hal and may be `anyhow` on IDF.
   - Link trait: `state`, `is_connected`, `is_suspended`, `suspend`, `resume`, `connect_count`, and async `wait_connected`.
   - MQTT client trait: `state`, `is_connected`, `connect_count`, `try_publish*`, async acknowledged publish, and async `receive`.
   - Async trait methods use native `async fn` in traits.
   - Futures are not required to be `Send`, because esp-hal futures are not.
5. Parity rules apply beyond the traits.
   - One type name per role in both crates, where the crate path carries the tier.
   - Same method names, arguments and return types on both tiers, and only the error type differs.
   - Shared value types come from `juggler` or `core`, for example `core::net::Ipv4Addr` and `&[u8]` payloads.
6. Enforcement uses three checks.
   - A parity compile check per tier compiles a function generic over the traits against each tier.
   - Host conformance tests run the same suite over the shared cores with mock ports, so a tier adapter has little left that only hardware can test.
   - One example pair, `idf_esp32s3_mqtt_lwt` and `hal_esp32s3_mqtt_lwt`, shares its application module.
7. Hardware differences that remain are documented as capabilities in one table.
   - The table starts from `docs/wifi-suspend-and-deep-sleep.md`.
   - A hardware difference is never expressed as a diverging API.
8. `WifiDriver` is deprecated in favour of the new traits.
9. The order is Wi-Fi and MQTT first (U2g), then provisioning, OTA, ESP-NOW and LoRa.
   - Each behaviour gap in the table above is its own feature.
10. Behaviour lives in host-testable cores in `juggler`; the tiers only implement narrow port traits (ports and adapters, the strategy pattern).
   - Precedents already in the code: the sans-IO state machines `LinkMachine` and `ScanMachine`, and `LinkSupervisor<R: LinkRadio>`, a generic core whose IDF port `IdfRadio` maps four `esp_wifi_*` calls and whose races are tested on the host against `MockRadio`.
   - The public traits of decision 4 are implemented once, generically, in `juggler` wherever the behaviour is the same on both tiers (for example an MQTT session core generic over an MQTT port), so both tiers behave the same by construction.
   - A port trait is narrow: one driver call or one event mapping per method, no decisions.
   - Time and wake-ups are injected: clocks as parameters, and `embassy-sync` primitives, which are `no_std` and run on the host.
   - Async cores are tested on the host with a small executor as a dev-dependency (for example `embassy-futures::block_on`).
   - Each tier keeps its own public type names through type aliases over the generic core (for example `pub type MqttHandle = MqttSession<IdfMqttPort>`), so consumers never spell the generic.
   - Review rule: a branch in a tier adapter that is not an error or event mapping is logic and moves into `juggler`.

## Consequences

### Positive

- Consumer adapters disappear, and the knob's `drivers/network.rs` shrinks to building the stack.
- One mental model covers both tiers.
- Drift is caught at compile time instead of on hardware.
- More behaviour is tested on the host.
- Tier behaviour cannot drift where the core is shared.

### Negative

- The IDF tier carries two surfaces during 0.x.
- IDF firmware needs an executor and an embassy-time driver.
- The extra stack and heap cost of the IDF async path is not measured yet.
- Embassy crate versions must unify across both tiers, and this is to be verified before the IDF async surface lands.
- The IDF async path needs a hardware campaign.
- Some API changes are breaking (IDF MQTT payload `&str` to `&[u8]`, IDF `is_connected` return type, IDF `with_startup_message` signature), so each needs a new name with deprecation or a deliberate 0.x minor break.
- One layer of indirection and generic public types, hidden behind per-tier type aliases.
- Some existing tier logic must be extracted first.
  - For example, the esp-hal MQTT runner's link watch and stall deadline.
  - For example, the IDF MQTT connection-epoch handling.

## Open Questions

- Which executor do the IDF examples use: embassy-executor with `arch-std`, or esp-idf-hal `block_on`?
- Does `embassy-time-driver 0.2.x` from `esp-idf-svc` unify with the workspace's `embassy-time`?
- Is each of the three breaking items a break or a deprecation?
- Does the blocking IDF API become thin wrappers over the async one?
- Answered 2026-11-01: the IDF async foundation is async wrappers over the existing worker threads (decision 2).

## Alternatives Considered

- **Keep per-tier adapters in each consumer** — this is the status quo that the maintainer rejected, since every consumer repeats the mapping and drift stays invisible.
- **Blocking API as the primary surface on both tiers** — esp-hal is built on an async executor, so a blocking surface there would poll the radio by hand.
- **One crate with a runtime-selected backend** — the tiers are mutually exclusive backends, which [ADR 005](005-crate-naming-for-dual-hal-drivers.md) keeps in separate crates.
- **Trait objects (`dyn` port)** — rejected because traits with `async fn` are not dyn-compatible and `no_std` would need an allocator for boxing.
- **Duplicated logic per tier** — the status quo, which let behaviour drift (suspend, reconnect latency).

## References

- [ADR 005](005-crate-naming-for-dual-hal-drivers.md), [ADR 016](016-crate-consolidation-for-publishing.md), [ADR 017](017-mqtt-callback-threading-contract.md), [ADR 019](019-esp-hal-async-mqtt-client.md), [ADR 020](020-esp-hal-espnow-owned-radio-and-sync-shim.md), [ADR 021](021-binary-owned-providers.md) — the tier and provider decisions this one builds on
- `docs/features/wifi-reconnect-and-suspend-v1.md` — the U2f feature doc
- `docs/wifi-suspend-and-deep-sleep.md` — the starting point for the capability table
- Upstream request U2g from `rustyfarian-knob` — one Wi-Fi + MQTT surface for both tiers
- Supersedes nothing.
