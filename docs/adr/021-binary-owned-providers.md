# ADR 021: Binary-Owned Providers — Console, Panic Handler and Critical Section Belong to the Firmware

## Status

Accepted — 2026-10-31 by the maintainer; the implementation lands with [esp-hal-binary-owned-console-and-panic-handler-v1.md](../features/esp-hal-binary-owned-console-and-panic-handler-v1.md), whose hardware gate (examples still print on a CrowPanel S3 and a C3 Super Mini) stays open.

## Context

Three global "providers" exist exactly once per firmware image: the `critical-section` implementation, the `esp-println` console and the `#[panic_handler]`.
Each is selected through Cargo features on a crate that the binary and its libraries share, so a library that selects one takes the choice away from the firmware and collides with the firmware's own selection.

Three findings in two days showed the workspace's library crates making that choice:

- Bug 007 ([007-lora-forces-critical-section-provider-2026-10-30.md](../bugs/007-lora-forces-critical-section-provider-2026-10-30.md)): the IDF `lora` feature enabled a `critical-section` provider, so a firmware that enabled another one hit `multiple definition of _critical_section_1_0_acquire`; the provider was removed from the feature.
- The `rustyfarian-knob` U2d request: the `esp32s3` chip feature of `rustyfarian-esp-hal-network` forwarded `esp-println?/uart`, and the knob's own `auto` console failed `esp-println 0.18`'s build script with `Currently enabled: uart, auto`.
- While fixing U2d: the `wifi` feature forced `esp-backtrace` with `panic-handler` into every firmware, although the library source references neither `esp_println` nor `esp_backtrace` — it never prints and never installs a panic handler.

An interim fix on 2026-10-31 stopped the chip features from selecting a console but kept both crates as optional dependencies of `wifi` and `lora`, which still forced the panic handler and made every lib-only check pass `esp-println/auto` on the command line.

## Decision

Amended 2026-10-31: scheduler added as the fourth provider (request U2e).

1. **Library crates of this workspace never select a provider.** `juggler`, `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` select neither a `critical-section` provider, nor an `esp-println` console, nor a panic handler, nor the `esp-rtos` scheduler; the firmware does (on bare metal `esp-hal` itself is the only provider and is not a Cargo feature, so nothing is selected there).
   A `#[cfg(test)]` provider that exists only in a crate's own unit-test binary (the hal crate's `host_critical_section`) is the binary's choice, not the library's.
2. **The `esp-rtos` scheduler starts once per boot, and the firmware decides who starts it.** `rustyfarian-esp-hal-network` starts it only when the firmware passes `TIMG0` and `FROM_CPU_INTR0` through `with_peripherals` or `with_ap_peripherals`, and `init_async` / `init_softap_async` do so before they validate the configuration.
   A firmware that already called `esp_rtos::start` passes only `WIFI` through `with_wifi_scheduler_running` or `with_ap_wifi_scheduler_running`, and the library starts nothing.
   Either way `esp_rtos::start` runs once per boot, because a second call panics in `esp-rtos`.
   `ProvisioningBuilder` offers the same pair for the captive portal: `start` starts the scheduler from `TIMG0` and `FROM_CPU_INTR0` as its first step, before its own profile, SSID and password checks, and `start_scheduler_running` takes only `WIFI`, so the portal fallback of a scheduler-owning firmware is covered.
3. **`rustyfarian-esp-hal-network` depends on neither `esp-println` nor `esp-backtrace`.** `wifi` and `lora` stop pulling them, the chip features forward nothing to them, and the docs.rs metadata lists no console.
4. **Examples are firmware.** The crate's examples take both crates from `[target.<triple>.dev-dependencies]` tables for the four triples (C3, C6, S3, classic ESP32), each carrying the chip, `auto` + `log-04` for `esp-println` and `panic-handler` + `println` for `esp-backtrace`.
   Only a target triple can carry the chip once the library stops forwarding it, and the host triple is excluded, so host tests never compile either crate.
5. **Lib-only checks need no console.** `just check-hal`, `just clippy-hal`, the docs.rs build and `just release-dry-run-hal` run with no console on the command line.

## Consequences

### Migration

A firmware that relied on the library for its console and panic handler adds both crates itself, with the chip and exactly one console (`auto`, `jtag-serial`, `uart` or `no-op`), plus `log-04` when it uses `esp_println::logger`:

```toml
[dependencies]
esp-println = { version = "=0.18.0", default-features = false, features = ["esp32s3", "auto", "log-04"] }
esp-backtrace = { version = "=0.20.0", default-features = false, features = ["esp32s3", "panic-handler", "println"] }
```

The binary links the panic handler with `use esp_backtrace as _;`.
A firmware with its own `#[panic_handler]` simply omits `esp-backtrace`.

A firmware that owns the scheduler starts it in `main` once per boot and passes only `WIFI` to the config, so the library starts nothing (the `rustyfarian-knob` boot shape, which must boot without Wi-Fi when no credentials exist):

```rust
use esp_hal::timer::timg::TimerGroup;
use rustyfarian_esp_hal_network::wifi::{WiFiConfig, WiFiConfigExt};

#[esp_rtos::main]
async fn main() {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    let config = WiFiConfig::new(env!("WIFI_SSID"), env!("WIFI_PASS"))
        .with_wifi_scheduler_running(peripherals.WIFI);
}
```

The captive-portal fallback of that firmware calls `.start_scheduler_running(spawner, peripherals.WIFI, store, rng)` on a `ProvisioningBuilder` instead of `.start(...)`, so the portal also starts nothing.

### Positive

- The three providers follow one rule across both tiers, and a custom panic handler no longer collides with a forced one.
- A `wifi` or `lora` consumer no longer compiles crates it never calls, and no lib-only check needs a console.
- A rejected Wi-Fi configuration no longer strands the firmware without a scheduler: with `with_peripherals`, `init_async` and `init_softap_async` start it before they validate, so a `WifiError::ConfigureFailed` leaves every embassy timer working.

### Negative

- Forgetting the console fails `esp-println`'s build script with `Currently enabled: none`.
- Forgetting the panic handler fails the link with `` `#[panic_handler]` function required, but not found ``; selecting `esp-backtrace/panic-handler` next to an own handler fails with `duplicate lang item panic_impl`.
- The IDF tier is unaffected by the console rule, since ESP-IDF owns the console; only the `critical-section` rule (bug 007) applies there.

## Alternatives Considered

- **Keep `esp-backtrace` as a `wifi` dependency without `println`** — still forces the panic handler, silently, and still collides with a firmware's own handler.
- **One `[target.'cfg(target_os = "none")'.dev-dependencies]` table for the examples** — cannot carry a chip, and `esp-println`'s build script needs exactly one chip feature.
- **Keep the interim command-line console** (`--features ...,esp-println/auto` in 24 justfile lines and the docs.rs metadata) — a crate that is no dependency cannot be passed as `dep/feature`, and the interim still forced the panic handler.

## References

- [bug 007](../bugs/007-lora-forces-critical-section-provider-2026-10-30.md) — the `critical-section` precedent on the IDF tier
- `docs/features/esp-hal-binary-owned-console-and-panic-handler-v1.md` — feature doc, decisions, constraints and gate
- `review-queue/archive/rustyfarian-network-u2d-esp-hal-println-uart-forward.md` — the `rustyfarian-knob` U2d request
- [ADR 006](006-no-std-esp-hal-wifi.md), [ADR 019](019-esp-hal-async-mqtt-client.md), [ADR 020](020-esp-hal-espnow-owned-radio-and-sync-shim.md) — the other esp-hal tier decisions
