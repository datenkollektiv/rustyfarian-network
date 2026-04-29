# Feature: OTA MVP — Three-Crate Dual-Stack Firmware Update v1

Add three new crates providing an end-to-end firmware update pipeline: `ota-pure` (platform-independent, no_std, host-testable), `rustyfarian-esp-idf-ota` (std, ESP-IDF), and `rustyfarian-esp-hal-ota` (bare-metal, no_std, async).

The feature is requested by `rustyfarian-ferriswheel-demo` (sibling repo) and aligns with `VISION.md` (OTA as firmware-update plumbing).
All public APIs are explicitly marked experimental; API stabilization is owned by the future `ota-library` feature.

**Locked by:**
- `review-queue/ota-mvp-three-crates.md` (feature request, submitted 2026-05-01)
- `docs/adr/011-ota-crate-hosting-and-transport.md` (Accepted; defines four critical decisions: in-workspace hosting, hand-rolled HTTP as internal transport, 1 MiB slots, rollback enabled + 30 s health criterion)

**Also see:**
- Consumer repo plan: `../rustyfarian-ferriswheel-demo/.claude/plans/we-are-on-a-rippling-metcalfe.md`
- Consumer repo feature doc: `../rustyfarian-ferriswheel-demo/docs/features/ota-mvp-v1.md`
- Beekeeper reference impl: `../rustyfarian-beekeeper/src/ota/` (lift sources verified 2026-05-01)

## Decisions

| Decision                       | Choice                                                                                                                                                                                                                                                                     | Reason                                                                                                                                                                                   | Rejected                                                                                                                                |
|:-------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------------------------------------|
| Crate hosting                  | Three crates live in `rustyfarian-network` workspace (`crates/ota-pure`, `crates/rustyfarian-esp-idf-ota`, `crates/rustyfarian-esp-hal-ota`)                                                                                                                               | Avoids coupling duplication with Wi-Fi/LoRa; leverages existing `*-pure` + `rustyfarian-esp-{idf,hal}-*` naming convention; extraction later is cheap (`git mv` + `Cargo.toml` rewrite). | Sibling `rustyfarian-ota` repo (rejected: OTA's dependency graph is dominated by networking crates this workspace already pins)         |
| HTTP transport on bare-metal   | Hand-rolled internal HTTP/1.1 GET client inside `rustyfarian-esp-hal-ota`; strict subset accepting only `HTTP/1.1 200 OK` + single valid `Content-Length`; rejects redirects, `Transfer-Encoding: chunked`, missing/duplicate headers, oversized bodies, incomplete reads. | Keeps "HTTP is out of scope" promise in workspace README; avoids premature `reqwless` adoption; small size budget for MVP demo binary.                                                   | `reqwless` or other off-the-shelf `no_std` HTTP client (rejected: contradicts README, adds dependency not justified by single use case) |
| Streaming SHA-256 verification | In MVP: lift `StreamingVerifier` to `ota-pure`, chunk-feed from download loop without holding full image in RAM                                                                                                                                                            | Catches corruption early, reduces flash churn on bad downloads, proven in beekeeper                                                                                                      | Defer to hardened: rejected (MVP requirement per consumer lock-in)                                                                      |
| Bare-metal flash writer        | `esp_bootloader_esp_idf::OtaUpdater` (already pinned in workspace at `=0.5.0`)                                                                                                                                                                                             | Direct partition-slot control; test-proven on bare-metal; aligns with esp-hal ecosystem                                                                                                  | Hand-rolled partition writer (rejected: redundant, breaks link to bootloader's rollback state machine)                                  |
| Public API stability           | All three crates explicitly mark public types as experimental; semver discipline deferred to `ota-library`                                                                                                                                                                 | Unblocks consumer demo delivery; allows implementation-detail pivots until Hardened                                                                                                      | Stabilize immediately (rejected: premature, consumer still iterating on error surfaces)                                                 |
| Lift verification strategy     | `Version`, `StreamingVerifier`, `bytes_to_hex`, `hex_to_bytes`, `OtaError` verified to exist in beekeeper `src/ota/` as of 2026-05-01; `ImageMetadata` and `OtaState` built fresh                                                                                          | Known-good codebases reduce risk; application-specific types (metadata sidecar parser, backend-neutral state machine) require custom design                                              | Rebuild everything from scratch (rejected: duplicates proven work in beekeeper)                                                         |

## Scope

**In scope:**

1. **`ota-pure` (new crate)** — platform-independent, no_std, host-testable, ~400 LOC
   - Semver parser and comparator (`Version`)
   - Streaming SHA-256 verifier (`StreamingVerifier`)
   - Fixed-size hex encoding/decoding (`bytes_to_hex`, `hex_to_bytes`)
   - Sidecar metadata parser (`ImageMetadata` for `.bin.sha256` and `.bin.version`)
   - Backend-neutral partition-swap state machine (`OtaState`)
   - Error enum with MVP variants only (`OtaError`: 8 variants)
   - No `std::error::Error` impl (deferred to std-only wrapper crates)

2. **`rustyfarian-esp-idf-ota` (new crate)** — ESP-IDF std, blocking, ~300 LOC
   - `OtaSession::new(config) -> Result<Self, OtaError>`
   - `session.fetch_and_apply(url: &str, expected_sha256: &[u8; 32]) -> Result<(), OtaError>` — stream download → verify → flash → swap in one pass
   - `session.mark_valid() -> Result<(), OtaError>` — cancel bootloader rollback
   - `session.rollback() -> Result<(), OtaError>` — revert to previous slot
   - Internally wraps `EspOta` / `EspOtaUpdate` from `esp-idf-svc`
   - HTTP fetched via `esp_idf_svc::http::client::EspHttpConnection` (lifted from beekeeper's `FirmwareDownloader`); HTTPS branch dropped per ADR 011 (plain HTTP only)

3. **`rustyfarian-esp-hal-ota` (new crate)** — bare-metal no_std, async-only, ~350 LOC
   - Public surface identical to IDF crate, with `async fn fetch_and_apply` / `mark_valid` / `rollback`
   - Internally wraps `esp_bootloader_esp_idf::OtaUpdater` over `esp-storage`
   - Hand-rolled async HTTP GET client over `embassy-net::TcpSocket` (strict subset, same validation as IDF)
   - Chip features: `esp32c3` (MVP), `esp32c6`, `esp32`; stack features: `unstable`, `rt`, `embassy`

**Out of scope (deferred to `ota-hardened` or `ota-library`):**

- TLS / HTTPS transport (plain HTTP only)
- Ed25519 signature verification (`SignatureVerifier` from beekeeper)
- Brown-out safety / power-loss recovery
- Automated rollback test in CI
- Multi-device concurrency / fleet rollout
- API stabilization, semver discipline, public docs polish
- `ota-pure` mock implementation
- `MqttManager` integration for OTA status topics (application concern)
- HTTP client extraction as workspace dependency (`reqwless` or sibling)

## Per-Crate Design

<details>
<summary><strong>ota-pure</strong></summary>

### File Layout

```
crates/ota-pure/
├── Cargo.toml
└── src/
    ├── lib.rs (re-exports)
    ├── version.rs (Version parser, Display, Ord)
    ├── verifier.rs (StreamingVerifier, bytes_to_hex, hex_to_bytes)
    ├── error.rs (OtaError enum, 8 variants)
    ├── metadata.rs (ImageMetadata parser)
    └── state.rs (OtaState enum, next_state helper)
```

### Public Surface (Experimental)

**Version parser:**
```rust
pub struct Version {
    major: u16,
    minor: u16,
    patch: u16,
}
impl Version {
    pub fn parse(s: &str) -> Result<Self, OtaError>;
    pub fn display(&self) -> impl Display;
    // impl Ord, PartialOrd, Eq, PartialEq
}
```

**Streaming verifier:**
```rust
pub struct StreamingVerifier {
    // sha2::Sha256 state
}
impl StreamingVerifier {
    pub fn new() -> Self;
    pub fn update(&mut self, chunk: &[u8]);
    pub fn finalize(self) -> [u8; 32]; // SHA-256 digest
}
```

**Hex helpers:**
```rust
pub fn bytes_to_hex(bytes: &[u8; 32]) -> heapless::String<64>;
pub fn hex_to_bytes(hex: &str) -> Result<[u8; 32], OtaError>;
```

**Image metadata (sidecar parser):**
```rust
pub struct ImageMetadata {
    pub sha256: [u8; 32],
    pub version: Version,
}
impl ImageMetadata {
    pub fn parse(sha256_hex: &str, version_str: &str) -> Result<Self, OtaError>;
}
```

**State machine:**
```rust
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OtaState {
    Idle,
    Downloading,
    Verifying,
    Writing,
    SwapPending,
    Booted,
}
impl OtaState {
    pub fn next_state(&self) -> Option<OtaState>;
}
```

**Error enum (MVP variants only):**
```rust
#[derive(Debug)]
pub enum OtaError {
    ServerUnreachable,
    DownloadFailed { status: u16 },
    DownloadTimeout,
    ChecksumMismatch,
    VersionInvalid,
    FlashWriteFailed,
    PartitionNotFound,
    InsufficientSpace,
}
```

### Cargo.toml

```toml
[package]
name = "ota-pure"
version = "0.1.0"
edition.workspace = true
# ... other workspace defaults

[dependencies]
sha2 = { workspace = true, default-features = false }
heapless = { workspace = true }

[dev-dependencies]
# None for MVP
```

### Lift Sources

- `Version` ← `../rustyfarian-beekeeper/src/ota/state.rs:7-79` (keep `core::fmt` impl, drop `String` in error paths for no_std)
- `StreamingVerifier` ← `verifier.rs:66-122` (adjust `bytes_to_hex` to return fixed-buffer `heapless::String<64>` instead of `String`)
- `bytes_to_hex` / `hex_to_bytes` ← `verifier.rs:169,199` (rework `bytes_to_hex` for fixed buffer)
- `OtaError` (trim to 8 MVP variants) ← `error.rs` (drop Wi-Fi / signature / power variants)
- `ImageMetadata` — **built fresh** (no equivalent in beekeeper; parses `.bin.sha256` hex string + `.bin.version` semver string)
- `OtaState` — **built fresh** (beekeeper's manager enum too coupled to async flow; new design is backend-neutral)

### Test Coverage

- Unit tests for `Version::parse` (valid/invalid semver strings)
- Roundtrip tests for `bytes_to_hex` + `hex_to_bytes`
- `StreamingVerifier` fed known-size chunks vs. one-shot reference SHA-256
- `OtaState::next_state` transitions via all states

---

</details>

<details>
<summary><strong>rustyfarian-esp-idf-ota</strong></summary>

### File Layout

```
crates/rustyfarian-esp-idf-ota/
├── Cargo.toml
├── build.rs (embuild::espidf::sysenv::output())
└── src/
    ├── lib.rs (re-exports + OtaSession + OtaSessionConfig)
    ├── downloader.rs (FirmwareDownloader wrapping EspHttpConnection, plain HTTP only)
    ├── flasher.rs (FirmwareFlasher + OtaWriter, wraps EspOta + EspOtaUpdate)
    └── session.rs (OtaSession::new, fetch_and_apply, mark_valid, rollback)
```

### Public Surface (Experimental)

**OTA session manager:**
```rust
pub struct OtaSession {
    // esp_idf_svc::ota::EspOta + config
}
impl OtaSession {
    /// Create a new OTA session from config.
    pub fn new(config: OtaSessionConfig) -> Result<Self, OtaError>;
    
    /// Fetch firmware from URL, verify SHA-256, write to inactive slot, swap slots.
    /// Streaming: never holds full image in RAM. Flash IS modified during the
    /// download (bytes are written to the inactive partition as they arrive).
    /// On any failure (download, verification, flash) the OTA write session is
    /// aborted and the boot slot is left unchanged — the device continues to
    /// boot the running image; the inactive partition is left in an undefined
    /// state and will be overwritten on the next OTA attempt.
    pub fn fetch_and_apply(
        &mut self,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError>;
    
    /// Mark the running slot valid, cancelling bootloader rollback.
    pub fn mark_valid(&self) -> Result<(), OtaError>;
    
    /// Revert to the previous OTA slot (if rollback is enabled).
    pub fn rollback(&self) -> Result<(), OtaError>;
}

pub struct OtaSessionConfig {
    pub timeout_secs: u64,
}
```

### HTTP Client

The IDF crate uses `esp_idf_svc::http::client::EspHttpConnection` directly — no hand-rolled parser is needed because `esp-idf-svc` already handles HTTP/1.1 framing, headers, and body length.
Plain HTTP only per ADR 011 (HTTPS branch present in beekeeper's `FirmwareDownloader` is removed; future TLS support belongs to `ota-hardened`).
Status code is checked explicitly: anything other than `200` returns `OtaError::DownloadFailed { status }`.

### Cargo.toml

```toml
[package]
name = "rustyfarian-esp-idf-ota"
version = "0.1.0"
edition.workspace = true

[build-dependencies]
embuild.workspace = true

[dependencies]
ota-pure.workspace = true
esp-idf-svc.workspace = true
embedded-svc = "0.28"  # for the Headers trait used by EspHttpConnection
log.workspace = true
```

### Lift Sources

- `FirmwareDownloader` ← `../rustyfarian-beekeeper/src/ota/downloader.rs:18-148` (keep HTTP client abstraction, adapt to blocking `TcpStream`)
- `FirmwareFlasher` + `OtaWriter` ← `flasher.rs` (1:1 onto `EspOta::initiate_update` / `EspOtaUpdate::write`)
- `create_http_connection` ← `manager.rs:469-521` (blocking socket version; drop HTTPS branch per ADR 011)

### Dropped from Beekeeper

- Battery guard, LoRaWAN dispatch, async flow `OtaState` enum, retry/backoff loop, Ed25519 verification
- `MqttManager` integration for lifecycle topics (app concern)

---

</details>

<details>
<summary><strong>rustyfarian-esp-hal-ota</strong></summary>

### File Layout

```
crates/rustyfarian-esp-hal-ota/
├── Cargo.toml
└── src/
    ├── lib.rs (re-exports + EspHalOtaManager)
    ├── http.rs (async HTTP/1.1 GET client over embassy-net, private)
    ├── manager.rs (EspHalOtaManager::new, fetch_and_apply, mark_valid, rollback)
    └── writer.rs (OtaUpdater::write wrapper)
```

### Public Surface (Experimental)

**Bare-metal OTA manager (async):**
```rust
pub struct EspHalOtaManager {
    // esp_bootloader_esp_idf::OtaUpdater
}
impl EspHalOtaManager {
    /// Create a new OTA manager from config.
    pub fn new(config: OtaManagerConfig) -> Result<Self, OtaError>;
    
    /// Fetch firmware from URL, verify SHA-256, write to inactive slot, swap slots.
    /// Async over embassy-net. Streaming: never holds full image in RAM.
    pub async fn fetch_and_apply(
        &mut self,
        socket: &mut embassy_net::tcp::TcpSocket<'_>,
        stack: &embassy_net::Stack<'_>,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError>;
    
    /// Mark the running slot valid, cancelling bootloader rollback.
    pub fn mark_valid(&self) -> Result<(), OtaError>;
    
    /// Revert to the previous OTA slot (if rollback is enabled).
    pub fn rollback(&self) -> Result<(), OtaError>;
}

pub struct OtaManagerConfig {
    pub timeout_secs: u64,
}
```

### HTTP Client (Private)

Module-level doc clearly states the same as IDF crate: internal transport, not public API, may be removed later.

Strict validation:
- Status line: `HTTP/1.1 200 OK`
- Exactly one `Content-Length` header
- Rejects redirects, `Transfer-Encoding: chunked`, missing/duplicate headers, oversized bodies
- Fixed-length streaming loop: EOF before exact `Content-Length` bytes = `DownloadTimeout`

### Cargo.toml

```toml
[package]
name = "rustyfarian-esp-hal-ota"
version = "0.1.0"
edition.workspace = true

[features]
esp32c3 = [
    "dep:esp-hal", "esp-hal/esp32c3",
    "dep:esp-bootloader-esp-idf", "esp-bootloader-esp-idf/esp32c3",
]
esp32c6 = [
    "dep:esp-hal", "esp-hal/esp32c6",
    "dep:esp-bootloader-esp-idf", "esp-bootloader-esp-idf/esp32c6",
]
esp32 = [
    "dep:esp-hal", "esp-hal/esp32",
    "dep:esp-bootloader-esp-idf", "esp-bootloader-esp-idf/esp32",
]
unstable = ["esp-hal/unstable"]
rt = ["esp-hal/rt"]
embassy = [
    "dep:embassy-net",
    "dep:embassy-time",
    "dep:static_cell",
    "dep:embedded-io-async",
]

[dependencies]
ota-pure.workspace = true
rustyfarian-network-pure.workspace = true
esp-hal = { workspace = true, optional = true }
esp-bootloader-esp-idf = { workspace = true, optional = true }
embedded-hal.workspace = true
log.workspace = true

# Embassy (opt-in via `embassy` feature)
embassy-net = { workspace = true, optional = true }
embassy-time = { workspace = true, optional = true }
static_cell = { workspace = true, optional = true }
embedded-io-async = { workspace = true, optional = true }
```

### Design Notes

- No blocking variant for MVP (async-only, requires `embassy` feature + chip feature)
- HTTP client threads received bytes through `embassy_net::TcpSocket::read(&mut buf)` loop
- Partition swapping deferred to `esp_bootloader_esp_idf::OtaUpdater::finalize()`
- No built-in retry or backoff (app concern; deferred to hardened)

---

</details>

## Workspace Changes

### `Cargo.toml` — Additions

**`[workspace.members]`:**
```
crates/ota-pure
crates/rustyfarian-esp-idf-ota
crates/rustyfarian-esp-hal-ota
```

**`[workspace.dependencies]`:**
```toml
sha2 = { version = "0.10", default-features = false }
```

(Verify `esp-idf-svc`, `esp-bootloader-esp-idf` are already pinned; no changes needed.)

### `justfile` — Recipes to Add

```sh
check-ota-pure:
  cargo check -p ota-pure --target "{{ host_target }}"

test-ota-pure:
  cargo test -p ota-pure --target "{{ host_target }}" --lib

check-ota-idf:
  cargo check -p rustyfarian-esp-idf-ota --target riscv32imc-esp-espidf

check-ota-hal:
  cargo check -p rustyfarian-esp-hal-ota --target "{{ host_target }}"

check-ota-hal-embassy:
  cargo check -p rustyfarian-esp-hal-ota --target riscv32imc-unknown-none-elf \
    --features esp32c3,unstable,rt,embassy -Zbuild-std=core,alloc
  cargo check -p rustyfarian-esp-hal-ota --target riscv32imac-unknown-none-elf \
    --features esp32c6,unstable,rt,embassy -Zbuild-std=core,alloc
```

**Extend `test` recipe:**
```sh
test: ... test-ota-pure ...
```

### `.github/workflows/rust.yml` — CI Lines

In the existing "Test pure crates" job, add:
```yaml
- name: Test ota-pure
  run: cargo test -p ota-pure --features mock --target "${{ env.host_target }}"
```

In the "Check HAL crates" job or similar, add:
```yaml
- name: Check rustyfarian-esp-idf-ota
  run: cargo check -p rustyfarian-esp-idf-ota --target riscv32imc-esp-espidf

- name: Check rustyfarian-esp-hal-ota (bare-metal)
  run: cargo check -p rustyfarian-esp-hal-ota --target riscv32imc-unknown-none-elf \
    --features esp32c3,unstable,rt,embassy -Zbuild-std=core,alloc
```

### `README.md` — Revision

Change the line currently reading:

> Out of scope: General-purpose application-layer clients (HTTP, CoAP, WebSocket) and provisioning/SoftAP flows. Feature-specific private transports may exist behind crate APIs, such as the OTA MVP's internal HTTP fetcher.

To:

> Out of scope: General-purpose application-layer clients (HTTP, CoAP, WebSocket) and provisioning/SoftAP flows. The OTA crates (`rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`) carry their own internal HTTP/1.1 GET clients for firmware download, but these are implementation details and not published as reusable workspace HTTP APIs.

### `ROADMAP.md` — Timing

Once Stage 3 lands (consumer-side bare-metal demo flows), move the Near-term `OTA MVP` item into the Completed section.

### `CHANGELOG.md` — New Entries

Under `## [Unreleased]`, add to the `### Added` section:

```markdown
- Three new crates for firmware OTA update:
  - `ota-pure` (platform-independent, no_std, host-testable) — `Version` parser, streaming `StreamingVerifier` (SHA-256), sidecar metadata parser (`ImageMetadata`), backend-neutral `OtaState` machine, `OtaError` enum (8 MVP variants)
  - `rustyfarian-esp-idf-ota` (ESP-IDF std, blocking) — `OtaSession::fetch_and_apply`, `mark_valid`, `rollback` with internal HTTP/1.1 GET client and streaming SHA-256 verification
  - `rustyfarian-esp-hal-ota` (bare-metal, no_std, async) — `EspHalOtaManager::fetch_and_apply`, `mark_valid`, `rollback` with async HTTP/1.1 GET client over `embassy-net` and `esp_bootloader_esp_idf::OtaUpdater` integration
  - All public APIs explicitly marked experimental; API stabilization is owned by the future `ota-library` feature
  - See ADR 011 for decisions on crate hosting (in-workspace), HTTP as internal transport (hand-rolled, strict subset), 1 MiB slot sizing, and rollback enablement with 30 s health criterion
  - CI: new `check-ota-*` and `test-ota-pure` recipes in justfile; `cargo test -p ota-pure` in workflow
```

## Stage Gates

| Stage | Owner                        | Trigger                                                                          | Pass Criteria                                                                                                                                                             | Blocker           |
|:------|:-----------------------------|:---------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:------------------|
| 1     | rustyfarian-network          | `ota-pure` implementation + tests land                                           | `just verify` clean; `cargo test -p ota-pure` passes on host; all public types have doc comments                                                                          | None              |
| 2     | rustyfarian-ferriswheel-demo | Consumer pulls `ota-pure` + `rustyfarian-esp-idf-ota` via path deps, builds demo | Demo fetches new firmware over Wi-Fi, validates SHA-256, swaps, reboots, marks valid; manual rollback test succeeds (serving truncated `.bin` triggers bootloader revert) | Stage 1 clean     |
| 3     | rustyfarian-ferriswheel-demo | Consumer pulls `rustyfarian-esp-hal-ota` via path dep, builds bare-metal demo    | Identical hardware behaviour to Stage 2 (fetch, verify, swap, mark valid, rollback) on same ESP32-C3 board; `just verify` clean in network repo                           | Stage 2 validated |

## State Checklist

- [ ] Design approved
- [ ] Stage 1 implemented (`ota-pure` crate, tests, justfile recipes)
- [ ] Stage 2 implemented (`rustyfarian-esp-idf-ota` crate, demo integration in consumer repo)
- [ ] Stage 3 implemented (`rustyfarian-esp-hal-ota` crate, bare-metal demo in consumer repo)
- [ ] `just verify` clean (formatting, clippy, deny)
- [ ] `just build-example` validates demo builds on hardware targets (ESP32-C3 IDF and bare-metal)
- [ ] `cargo doc --no-deps -p ota-pure -p rustyfarian-esp-idf-ota -p rustyfarian-esp-hal-ota --open` shows all public types documented
- [ ] README and ROADMAP updated
- [ ] CHANGELOG entries confirmed (keep-a-changelog format)

## Open Questions

None identified as of 2026-05-01.
The four ADR 011 decisions fully constrain the design; consumer feature doc and beekeeper reference impl cover all ambiguities.

## Session Log

**2026-05-01** — Feature design doc created from `review-queue/ota-mvp-three-crates.md`, locked by ADR 011, beekeeper lift inventory completed. Four crates' public surface, Cargo.toml patterns, lift sources, HTTP strictness constraint, stage gates, and workspace integration all specified. Ready for Stage 1 implementation.
