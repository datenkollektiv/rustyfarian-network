# ADR 019: Async MQTT Client for the esp-hal Tier

## Status

Accepted — 2026-10-14 (client, shared types, runtime model); the maintainer chose IP-literal broker addresses over DNS.

The canonical sign-off anchor is the State checklist in [esp-hal-mqtt-v1.md](../features/esp-hal-mqtt-v1.md).

## Context

`docs/ROADMAP.md` gates the bare-metal MQTT domain behind an "async MQTT decision ADR" and names `minimq` as the intended base.
Three consumers now wait on it: the rgb-puzzle brain (ESP32-C6), `rustyfarian-rgb-clock` (ADR 015) and `rustyfarian-knob` (ESP32-S3, request U2b).
The bare-metal estate is async-only: `esp-radio 1.0.0-beta.1` exposes Wi-Fi solely through `embassy-net` (ADR 015, lore "esp-hal Bare-Metal Stack").

The estate pins the September 2026 wave: `esp-hal =1.2.2`, `esp-radio =1.0.0-beta.1`, `esp-rtos =0.4.0`, `embassy-net =0.8.0`, `embassy-time =0.5.1`, `embassy-sync =0.8.0`, `heapless 0.9`.
Any client must bind those versions without a second copy of a trait-bearing crate on the public boundary.
`embassy-net 0.8` itself depends on `embassy-sync 0.7.2` and `embedded-io-async 0.7.0`, so both `embassy-sync` majors already coexist in `Cargo.lock`; the real constraint is that no client may hand *us* `embassy-sync 0.7` types.

The ESP-IDF tier's behaviour is the parity target (ADR 017, bug 005): LWT, a retained "online" publish on every (re)connect, resubscribe after reconnect, auto-reconnect, and a non-blocking `try_publish` that reports `WouldBlock`.
Its callback contract does not translate: there are no helper threads on a single-core embassy executor, so ADR 017's `WrongThread` guard has no equivalent here.

On the S3, `esp-radio` and `esp-rtos` allocate only from internal RAM (`malloc_internal`), and the validated S3 Wi-Fi example leaves ~26 KB of a 72 KiB heap free (lore, 2026-10-14).
The MQTT layer must therefore not grow the heap footprint.

## Decision

### 1. Client: `minimq =0.13.5`

`minimq` is an async, `no_std`, no-alloc MQTT 5 client driven over an `embedded_io_async::Read + Write` byte stream.
Keepalive, QoS 0/1/2 handshakes and in-flight replay live inside the client; transport setup, reconnect timing and dispatch stay with us, which is exactly the split the runner below needs.
Pinned exactly, like the rest of the wave: the 0.13 line is five weeks old with five patch releases, and 0.13.5 changed flush semantics on 2026-10-11.
Declared with `default-features = false`: `minimq`'s default feature is `defmt`, which would drag a `defmt` global logger requirement into every binary; without it the crate's log macros compile to no-ops.

| Candidate | Deps added | Alloc | Protocol | Connection mgmt | Verdict |
|:----------|:-----------|:------|:---------|:----------------|:--------|
| **minimq 0.13.5** | `embassy-time 0.5.1`, `embedded-io-async 0.7`, `heapless 0.9`, `num_enum`, `serde`, `thiserror 2` | none (caller buffers) | MQTT 5 | keepalive, replay, Will inside | **Chosen** |
| rust-mqtt 0.6.0 | `embedded-io-async 0.7`, `heapless 0.9`, `const_fn`; `alloc` or `bump` buffer provider | optional `alloc` | MQTT 5 | none by design (README) | Rejected |
| hand-rolled over `mqttrs 0.4.1` | `mqttrs`, `heapless 0.7` (third copy) | none | MQTT 3.1.1 | all ours (~900 lines) | Runner-up |

Both published candidates are MQTT 5 only.
Mosquitto ≥ 1.6 and every broker the three consumers run speak MQTT 5, so this is recorded as a constraint, not a blocker.

### 2. Transport: `embassy-net` `TcpSocket`, broker address as an IPv4 literal

`TcpSocket<'_>` implements `embedded_io_async 0.7` `Read`/`Write` (`embassy-net-0.8.0/src/tcp.rs:712,724`), the same trait version `minimq` binds, so no adapter is needed.
`MqttConfig::host` must be an IPv4 literal on this tier; the builder rejects anything else with `MqttError::InvalidConfig` at build time, not at connect time.
`embassy-net/dns` stays off: it would add a DNS socket and ~1 KiB to every `embassy` build of the crate, and no bare-metal consumer needs a hostname yet.
Enabling it later is additive (`Stack::dns_query` already short-circuits literals, `embassy-net-0.8.0/src/lib.rs:536`) and would need `StackResources<3>` → `<4>` in `wifi/mod.rs`.

### 3. Runtime model: one runner task, channels instead of callbacks

A single `mqtt_runner` task owns the `minimq::Session`, the socket and all buffers.
It loops: resolve → TCP connect (bounded by `connection_timeout_ms`) → `Session::connect` → on `ConnectEvent::Connected` publish the configured startup message and every builder subscription → drive `Connection::poll()` in a `select` with the publish queue → on any error drop the connection, mark disconnected, wait `reconnect_timeout_ms` (default 10 s, matching the IDF tier), repeat.
Sessions use the default clean start (`session_expiry_interval = 0`), so every connect is `Connected` and resubscribes; broker-side session resumption is out of scope.

Application code talks to the runner through `embassy-sync 0.8` primitives on a `'static` state block:
- `MqttHandle::try_publish*` pushes onto a bounded publish `Channel`; a full queue or a disconnected session returns `TryPublishError::WouldBlock`.
- `MqttHandle::publish*` awaits queue space; `MqttHandle::receive().await` / `try_receive()` take owned inbound messages from a bounded channel that drops the oldest on overflow.
- `MqttHandle::state()` reads an atomic `MqttConnectionState`; `wait_connected().await` uses a `Signal`.

No closure runs inside the client, so ADR 017's `WrongThread` class of bugs cannot occur; its `on_connect` role is covered by the startup message and the builder subscriptions.

### 4. Shared types move to `juggler` without the `std` gate

`juggler::mqtt::QoS` loses its `#[cfg(feature = "std")]`.
`MqttConfig<'a>` and `LwtConfig<'a>` move from `rustyfarian-esp-idf-network` into `juggler::mqtt` (with `core::fmt`, redaction kept); the IDF tier re-exports them, so its public names do not change.
`juggler::mqtt::TryPublishError<E>` becomes the shared shape (`WouldBlock | Other(E)`); the IDF tier keeps `pub type TryPublishError = TryPublishError<anyhow::Error>`, and the HAL tier uses its own error type for `E`.
`task_stack_size` stays on the shared `MqttConfig` and is documented as IDF-only; the HAL runner ignores it.

### 5. Memory: static buffers, zero heap

All MQTT memory is `StaticCell`-backed `.bss`: `minimq` RX 1 KiB and TX 2 KiB arenas, TCP RX/TX 1536 B each, a 4-deep publish queue and a 4-deep inbound queue with `heapless` payloads (topic ≤ 64 B, payload ≤ 256 B).
Roughly 8 KiB static, no allocator traffic, so the S3 heap headroom measured for Wi-Fi is unchanged; the hardware run records the figure in lore.
Sizes are crate constants in v1, following the `StaticCell` lore entry; const generics on the builder are the post-1.0 path.

### 6. Chips and scope

Every `mqtt` cfg arm covers `esp32c3`, `esp32c6` and `esp32s3` from the first commit, mirroring the Wi-Fi arms.
Out of scope for v1: TLS, QoS 2 publishes, runtime (un)subscribe after build, persistent sessions, and STA + ESP-NOW + MQTT coexistence (tracked in `esp-hal-espnow-v1.md`).

## Consequences

### Positive

- One dependency family already in the pinned wave; `cargo tree` on the scratch crate (`tmp/adr-mqtt-resolve`) resolves `minimq 0.13.5` next to `esp-radio 1.0.0-beta.1` with `embassy-sync 0.8.0` untouched.
- No heap growth and no callbacks, so the two IDF-tier failure classes (bug 001 deadlock, callback misuse) are structurally absent.
- One config surface for both tiers; consumers keep `MqttConfig::new(host, port, id).with_lwt(...)` verbatim.

### Negative

- MQTT 5 only; a consumer on a 3.1.1-only broker cannot use the HAL tier.
- `minimq 0.13` is young; each patch bump needs a re-run of the hardware script until a 1.0 lands.
- Fixed queue sizes mean large payloads (> 256 B) are rejected with `TryPublishError::Other` in v1.
- IPv4-literal broker addresses only; a consumer that ships a hostname in `MQTT_HOST` must resolve it elsewhere or wait for the DNS follow-up.

## Alternatives Considered

- **rust-mqtt 0.6.0** — resolves and type-checks on the same scratch crate, but leaves keepalive, reconnects and retries to the caller by design, needs an `alloc` or `bump` buffer provider, and exposes a nine-parameter `Client` type; it is a protocol toolkit, not a managed session.
- **Hand-rolled MQTT 3.1.1 over `mqttrs`** — zero behavioural surprises and 3.1.1 support, but ~900 lines of session code to own plus a third `heapless` major; kept as the ADR 015-style runner-up if `minimq` fails the hardware script.
- **`embassy-net/dns` on the `embassy` feature** — deferred by the maintainer; it taxes every embassy build for a need no consumer has yet, and it stays an additive follow-up.
- **Callback API parity with the IDF tier** — rejected (feature doc decision); a single-core executor has no thread to run callbacks on.

<details>
<summary>Source evidence</summary>

- `tmp/adr-mqtt-resolve/Cargo.lock`: `minimq 0.13.5` and `rust-mqtt 0.6.0` lock next to `esp-hal 1.2.2`, `esp-radio 1.0.0-beta.1`, `esp-rtos 0.4.0`, `embassy-net 0.8.0` (`dns` on), `embassy-sync 0.8.0`; `cargo check --target riscv32imc-unknown-none-elf -Zbuild-std=core,alloc` type-checks both clients.
- `minimq-0.13.5/Cargo.toml`: deps `embassy-time ^0.5.1`, `embedded-io-async ^0.7`, `heapless ^0.9.1`, `num_enum`, `serde`, `thiserror 2`; `license = "MIT"`; `rust-version = 1.88`.
- `minimq-0.13.5/src`: no `alloc::`/`Box`/`Vec` outside `heapless` (`grep`); `README.md` "no_std, no-alloc, async MQTT 5 client".
- `minimq-0.13.5/src/mqtt_client/session/drive.rs:139`: `poll()` is cancel-safe given a cancel-safe transport; `operations.rs:160`: QoS 1/2 publishes cancel-safe, QoS 0 is not — the runner finishes each publish before re-entering `select`.
- `minimq-0.13.5/src/lib.rs:176`: `Error::NotReady` when the TX arena is full; `src/will.rs:34-58`: `Will::new(topic, data, props).retained().qos(..)`; `src/config.rs:112,132`: `auth`, `keepalive_interval`.
- `rust-mqtt-0.6.0/README.md`: "does not implement opinionated connection management — automatic reconnects, keepalive loops, retry policies, or background tasks are intentionally left to the user"; "only MQTT version 5.0 is supported".
- `embassy-net-0.8.0/Cargo.toml:462-467`: `embassy-sync 0.7.2`, `embedded-io-async 0.7.0`; `src/tcp.rs:712,724`: `Read`/`Write` impls for `TcpSocket`.
- `esp-radio-1.0.0-beta.1/src/compat/malloc.rs`, `esp-rtos-0.4.0/src/lib.rs:172`: radio and scheduler allocate via `malloc_internal` / `InternalMemory` only.

</details>

## References

- `docs/features/esp-hal-mqtt-v1.md` — feature doc and sign-off anchor.
- `review-queue/feature-request-esp-hal-mqtt-esp32s3.md` — knob request U2b (S3 as first-class chip).
- [ADR 015](015-esp-hal-provisioning.md) — bare-metal estate constraints and the hand-rolled fallback rule.
- [ADR 017](017-mqtt-callback-threading-contract.md) — IDF-tier callback contract this ADR deliberately does not port.
- `docs/project-lore.md` § "esp-hal Bare-Metal Stack", § "MQTT Event Loop".
