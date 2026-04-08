# Embassy Integration Research

Research into async Wi-Fi support for `rustyfarian-esp-hal-wifi` using the
embassy ecosystem.
Conducted 2026-03-20 against esp-hal 1.0, esp-radio 0.17, esp-rtos 0.2.

## Current architecture (blocking)

```
WiFiManager::init()
  ├─ esp_alloc::HEAP.add_region()      (72 KiB)
  ├─ esp_rtos::start(timer, sw_int)    (starts RTOS scheduler)
  ├─ esp_radio::init()                 (radio hardware)
  ├─ esp_radio::wifi::new()            (WiFi controller + device)
  ├─ configure() / start() / connect() (STA mode)
  └─ returns WiFiManager

WiFiManager::wait_connected(timeout_ms)
  ├─ loop { controller.is_connected()? }  (blocking L2 poll)
  ├─ smoltcp::Interface + dhcpv4::Socket   (manual DHCP)
  ├─ loop { iface.poll(); socket.poll() }  (blocking DHCP poll)
  └─ returns Ipv4Address
```

All code runs in the main thread.
`esp_rtos` provides a preemptive scheduler via timer interrupts so the WiFi
firmware blob's internal tasks can run.

## What embassy brings

Embassy replaces manual poll loops with cooperative async tasks:

| Concern       | Blocking (now)             | Embassy (proposed)                                       |
|:--------------|:---------------------------|:---------------------------------------------------------|
| WiFi connect  | `loop { is_connected()? }` | `controller.connect_async().await`                       |
| DHCP          | Manual smoltcp polling     | `embassy_net::Config::dhcpv4()` — automatic              |
| Socket I/O    | Not implemented            | `TcpSocket::new(stack, ...)` with async read/write       |
| LED animation | Inline in poll loop        | Separate async task, concurrent with WiFi                |
| Runtime       | `esp_rtos::start()`        | `#[esp_rtos::main]` — same RTOS, embassy executor on top |

## Options explored

<details>
<summary><strong>Option A: Embassy-native WiFiManager (full async)</strong></summary>

Replace `WiFiManager` with an async version that spawns embassy tasks.

### How the example code would look

```rust
#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use embassy_executor::Spawner;
use embassy_net::{Config, Stack, StackResources};
use embassy_time::{Duration, Timer};
use esp_println::println;
use rustyfarian_esp_hal_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = env!("WIFI_SSID");
const PASSWORD: &str = env!("WIFI_PASS");

#[esp_rtos::main]
async fn main(spawner: Spawner) -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Two calls, one global esp_alloc::HEAP with two backing regions.
    // Region 1 — reclaimed IRAM (64 KiB, DMA-accessible): ROM/bootloader
    //   memory freed after esp_hal::init(). WiFi RX/TX ring buffers require
    //   DMA-accessible SRAM, so this region must exist before esp_radio::init().
    // Region 2 — regular DRAM (36 KiB): general-purpose heap (Box, Vec, etc.).
    // ESP32-C6's SRAM banks are physically separate; ESP32-C3 has one contiguous
    // block and uses a single call (72 KiB) instead.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let wifi = WiFiManager::init_async(
        WiFiConfig::new(SSID, PASSWORD)
            .with_peripherals(peripherals.TIMG0, peripherals.SW_INTERRUPT, peripherals.WIFI),
    ).unwrap();

    spawner.spawn(wifi_task(wifi.controller)).ok();
    spawner.spawn(net_task(wifi.runner)).ok();

    loop {
        if let Some(config) = wifi.stack.config_v4() {
            println!("IP: {}", config.address.address());
            break;
        }
        Timer::after(Duration::from_millis(500)).await;
    }

    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        controller.connect_async().await.ok();
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
    }
}

#[embassy_executor::task]
async fn net_task(runner: Runner<'static, WifiDevice<'static>>) {
    runner.run().await;
}
```

### What WiFiManager would provide

```rust
pub struct AsyncWifiHandle {
    pub controller: WifiController<'static>,
    pub stack: &'static Stack<WifiDevice<'static, WifiStaDevice>>,
    pub runner: Runner<'static, WifiDevice<'static, WifiStaDevice>>,
}

impl WiFiManager {
    pub fn init_async(config: HalWifiConfig<'_>) -> Result<AsyncWifiHandle, WifiError> {
        // Same heap/rtos/radio init as today, returns components to spawn
    }
}
```

### Trade-offs

- **Pro**: Proper concurrency — LED, WiFi reconnection, and DHCP as independent tasks
- **Pro**: No manual smoltcp polling — `embassy-net` handles it
- **Pro**: Clean async/await code, no `delay.delay_millis()` busy-waits
- **Con**: Requires `#[esp_rtos::main]` entry point (async main)
- **Con**: Users must understand embassy task spawning
- **Con**: `'static` lifetime requirements for tasks are infectious — needs `mk_static!` or `Box::leak`

</details>

<details>
<summary><strong>Option B: Keep blocking WiFiManager, add async companion</strong></summary>

Keep the existing blocking `WiFiManager` for simple use cases.
Add a separate `WiFiManagerAsync` for embassy users.

```rust
// Simple blocking usage (unchanged)
let mut wifi = WiFiManager::init(config)?;
let ip = wifi.wait_connected(30_000)?;

// Async usage (new)
let handle = WiFiManagerAsync::init(config)?;
spawner.spawn(handle.connection_task()).ok();
spawner.spawn(handle.net_task()).ok();
let ip = handle.wait_for_ip().await;
```

### Trade-offs

- **Pro**: No breaking change to existing API
- **Pro**: Users choose blocking or async based on their needs
- **Con**: Two code paths to maintain
- **Con**: `init_inner` would need to branch or be duplicated

</details>

<details>
<summary><strong>Option C: Async-first with blocking wrapper</strong></summary>

Make the core implementation async, then provide a blocking wrapper
that runs the async code on a single-shot executor.

```rust
impl WiFiManager {
    async fn connect_async(&mut self) -> Result<(), WifiError> {
        self.controller.connect_async().await.map_err(WifiError::Driver)
    }

    pub fn wait_connected(&mut self, timeout_ms: u64) -> Result<Ipv4Address, WifiError> {
        embassy_futures::block_on(self.wait_connected_async(timeout_ms))
    }
}
```

### Trade-offs

- **Pro**: Single implementation, two interfaces
- **Pro**: Async-first is the ecosystem direction
- **Con**: `block_on` in embedded is tricky — needs the executor to be running
- **Con**: `embassy_futures::block_on` only works for simple futures, not multi-task scenarios

</details>

<details>
<summary><strong>Option D: Provide building blocks, not a manager</strong></summary>

Expose initialized components and let users compose them.

```rust
let components = wifi_init(config)?;

// User A: blocking with smoltcp
let mut mgr = BlockingWifiManager::from(components);
let ip = mgr.wait_connected(30_000)?;

// User B: embassy-net
let (stack, runner) = embassy_net::new(components.sta_device, ...);
spawner.spawn(net_task(runner)).ok();
```

### Trade-offs

- **Pro**: Maximum flexibility
- **Pro**: No opinion on async vs blocking baked into the library
- **Con**: More boilerplate for users
- **Con**: Less "batteries included"

</details>

## LED animation with embassy

The biggest win for embassy is concurrent LED animation.
Currently, the LED pulse is interleaved with WiFi polling in the same loop.
With embassy, it becomes a separate task:

```rust
#[embassy_executor::task]
async fn led_task(mut led: impl StatusLed, state: &'static WifiState) {
    let mut pulse = PulseEffect::new();
    loop {
        let color = match state.get() {
            State::Connecting => pulse.update(WIFI_CONNECTING),
            State::Connected => RGB8::from(CONNECTED),
            State::Error => pulse.update(ERROR),
        };
        let _ = led.set_color(color);
        Timer::after(Duration::from_millis(50)).await;
    }
}
```

This decouples LED animation from WiFi polling — smoother animation,
cleaner code, and the LED keeps pulsing even during long DHCP negotiation.

## Recommendation

**Option B (blocking + async companion)** is the pragmatic choice:

1. The blocking API works and is validated on hardware (ESP32-C3)
2. Embassy support can be added incrementally without breaking existing users
3. The `take_sta_device()` method already enables embassy-net integration by downstream code
4. A full async rewrite (Option A/C) should wait until `esp-radio` is stable on all chips (C6 bug pending)

### Incremental path

1. **Now**: Keep blocking `WiFiManager` as-is (working on C3)
2. **Next**: Add `embassy` feature flag to `rustyfarian-esp-hal-wifi`
3. **Next**: Add `WiFiManagerAsync` behind the feature flag
4. **Next**: Add embassy examples (`hal_c3_connect_async`)
5. **Later**: If embassy becomes the primary use case, consider Option C (async-first)

## New dependencies required (all optional, behind `embassy` feature)

| Crate               | Version | Purpose                                  |
|:--------------------|:--------|:-----------------------------------------|
| `embassy-executor`  | 0.9     | Task spawner                             |
| `embassy-net`       | 0.7     | Async network stack (wraps smoltcp)      |
| `embassy-time`      | 0.5     | Async timers (`Timer::after`)            |
| `static_cell`       | 2.1     | Safe `'static` allocation for task state |
| `embedded-io-async` | 0.6     | Async I/O traits for sockets             |

`esp-rtos` already supports embassy via its `embassy` feature.

## Open questions

- Should the `WifiDriver` trait in `wifi-pure` get async counterparts (`connect_async`, etc.)?
- Should LED animation always use embassy tasks, or keep the inline approach for blocking mode?
- Is `esp-rtos` with `embassy` feature stable enough for production on ESP32-C3?
