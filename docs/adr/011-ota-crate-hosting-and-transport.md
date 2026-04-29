# ADR 011: OTA Crate Hosting and Transport Decisions

## Status

Accepted

## Context

The `rustyfarian-ferriswheel-demo` project plans an OTA MVP (see
`docs/features/ota-mvp-v1.md` in that repo) covering dual-stack firmware
update on ESP32-C3: an `esp-idf-svc::ota::EspOta`-based path and a bare-metal
path that drives `esp-bootloader-esp-idf::OtaUpdater` over `esp-storage`.
The feature doc commits to three crates following the established
`*-pure` + `rustyfarian-esp-{idf,hal}-*` triad: `ota-pure`,
`rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`.

Four decisions need to be locked before implementation work begins:

1. **Where do the OTA crates live** — a new sibling repo `rustyfarian-ota`,
   matching the `rustyfarian-ws2812` precedent, or inside the existing
   `rustyfarian-network` workspace alongside the `wifi`/`lora`/`espnow`
   triads.
2. **HTTP transport on the bare-metal stack** — adopt `reqwless` (or
   another off-the-shelf `no_std` HTTP/1.1 client) and surface it as a
   reusable workspace dependency, or hand-roll a minimal client inside
   `rustyfarian-esp-hal-ota` and treat HTTP as an internal transport
   detail rather than a public surface of `rustyfarian-network`.
3. **OTA partition slot sizing for the MVP demo** — 1 MiB per slot
   (conservative) versus about 1.93 MiB per slot (generous, leaving only
   `nvs` + `otadata` outside).
4. **Rollback enablement and `mark_valid` health criterion for the MVP demo** —
   which bootloader/sdkconfig settings are required, and what the running
   firmware must observe before calling the stack-specific `mark_valid` API
   to cancel the bootloader rollback.

The README of `rustyfarian-network` currently states explicitly:
"Out of scope: General-purpose application-layer clients (HTTP, CoAP,
WebSocket) and provisioning/SoftAP flows." This wording constrains decision
(2): the OTA crate may use a private HTTP transport, but must not expose a
reusable HTTP client as part of `rustyfarian-network`.

The constraints `esp-hal = "=1.0.0"` and `esp-bootloader-esp-idf = "=0.4.0"`
are load-bearing across the workspace (see the demo's
`docs/project-lore.md` "Boot & Flash") and frame the dependency surface
the OTA crates can introduce.

## Decision

### 1. The three OTA crates live inside `rustyfarian-network`

`ota-pure`, `rustyfarian-esp-idf-ota`, and `rustyfarian-esp-hal-ota` are
added to `crates/` in this workspace alongside the existing `wifi`,
`lora`, and `espnow` triads. No new sibling repository is created.

### 2. HTTP/1.1 GET is hand-rolled inside `rustyfarian-esp-hal-ota` as an internal transport

The bare-metal OTA crate carries its own minimal HTTP/1.1 client that
issues a single `GET` over an `embassy-net::TcpSocket` and streams the
response body to the partition writer. The client is not exported, not
re-exported, and not advertised as a reusable HTTP transport. It is an
implementation detail of `rustyfarian-esp-hal-ota`.

The MVP client is deliberately strict. It accepts only `HTTP/1.1 200 OK`
responses with exactly one valid `Content-Length` header. It rejects
redirects, `Transfer-Encoding: chunked`, missing or duplicate
`Content-Length`, bodies larger than the inactive OTA partition, and any
connection that reaches EOF before exactly `Content-Length` body bytes have
been streamed. Unsupported response shapes fail the OTA attempt before the
boot partition is changed.

`reqwless` and other off-the-shelf `no_std` HTTP clients are explicitly
not adopted at the workspace level. The README's "general-purpose HTTP
clients are out of scope" note remains accurate; the private OTA transport
does not become a workspace HTTP API.

### 3. OTA partition slots are 1 MiB each in the MVP demo reference layout

The reference partition table the demo binaries flash uses two app
partitions of `0x100000` bytes each (`ota_0` and `ota_1`), an `otadata`
partition of `0x2000` bytes, an `nvs` partition of `0x4000` bytes, and no
`factory` partition. A `phy_init` partition is intentionally omitted for the
MVP reference layout; the demo relies on the ESP-IDF/bare-metal defaults
already used by the Wi-Fi crates instead of reserving a separate PHY data
partition. This sizing fits comfortably inside the 4 MiB flash of an
ESP32-C3 SuperMini and reserves headroom for the eventual `ota-hardened-v1`
artefacts (signature blocks, metadata).

The exact demo `partitions.csv` is owned by `rustyfarian-ferriswheel-demo`,
but must be checked into that repo and used by both IDF and bare-metal
examples. Its offsets must be 4 KiB aligned, both app slots must be exactly
1 MiB, and the build/flash recipe must fail if the target flash size is
smaller than 4 MiB or if the produced image is larger than the selected OTA
slot. First serial flash uses the same partition table, erases `otadata`,
boots the first available OTA slot (`ota_0`), and marks that baseline image
valid before any rollback test is attempted.

The OTA library crates remain partition-agnostic: they parse the
partition table at runtime via `esp-bootloader-esp-idf::OtaUpdater` and
do not encode slot sizes or offsets in their public API. The 1 MiB
figure is a property of the demo firmware, repeated here so the demo's
feature doc and this ADR agree.

### 4. The MVP demo enables bootloader rollback and marks the running image valid after Wi-Fi association plus a 30 s wall-clock dwell

Rollback is a hard MVP requirement, not a best-effort behavior. The demo
sdkconfig enables `CONFIG_BOOTLOADER_APP_ROLLBACK_ENABLE=y`; flash recipes
must use a bootloader built with that setting when the target stack requires
an explicit bootloader image. The recipe also erases or initializes
`otadata` intentionally during first flash so both stacks start from the same
slot-selection state.

The demo binary calls the stack-specific `mark_valid` API
(`EspOta::mark_running_slot_valid()` on the IDF stack, which wraps
`esp_ota_mark_app_valid_cancel_rollback()`; the equivalent
`OtaUpdater::set_current_ota_state(OtaImageState::Valid)` path on the
bare-metal stack) once both of the following hold:

- The Wi-Fi station has associated and acquired an IPv4 lease.
- 30 seconds have elapsed since boot.

This is a deliberately minimal health signal sufficient for the MVP demo.
It proves that the image booted, the Wi-Fi credentials still work, and the
application remained alive for a short dwell period; it does not prove that
the OTA fetch path still works. The OTA library crates do not enforce any
health criterion, do not provide a timer, and do not own the rollback
policy. The library exposes the `mark_valid` operation; the application
decides when to call it.

The criterion is documented here so the bare-metal and IDF demo binaries
behave identically; richer criteria (heartbeat to the host, application
self-test, watchdog integration) are deferred to `ota-hardened-v1`.

## Rationale

### On crate hosting

The `rustyfarian-ws2812` precedent argued for a separate repository per
peripheral family. Applied here it would produce `rustyfarian-ota`. We
considered that path and rejected it for three reasons.

First, OTA's runtime dependency surface is dominated by what
`rustyfarian-network` already provides: Wi-Fi association
(`rustyfarian-esp-idf-wifi` / `rustyfarian-esp-hal-wifi`) and the
underlying radio/Embassy plumbing those crates already pin. A standalone
`rustyfarian-ota` repo would either duplicate those pins or pull
`rustyfarian-network` as a git dependency from itself — extra coupling
without extra value.

Second, the naming triad is already established here. `wifi-pure`,
`lora-pure`, and `espnow-pure` set the convention; `ota-pure` slots in
without explanation. Discoverability for downstream consumers improves
when "the rustyfarian thing for X" is one repo lookup, not two.

Third, the cost of extracting later if needed is small: the three
crates are independently versionable workspace members, and a future
move to a sibling repo is a `git mv` plus a `Cargo.toml` rewrite, not
an architectural change.

Naming honesty: this expands the scope of `rustyfarian-network` from
"Wi-Fi, MQTT, LoRa, ESP-NOW" to also include OTA. The README and
`VISION.md` are updated to reflect this expansion when ADR 011 lands.

### On HTTP being internal, not a workspace dependency

The README's explicit "out of scope: HTTP" line is a feature, not an
oversight. It signals that this workspace ships *connectivity* — radios
and link-layer plumbing — and leaves application protocols to consumer
crates. Adopting `reqwless` workspace-wide would either silently
contradict that line or force a scope expansion the workspace does not
benefit from.

A hand-rolled client for one `GET` request is small because the accepted
HTTP subset is tiny: one status line, ordinary headers, one
`Content-Length`, and a fixed-length body-streaming loop. Everything else is
an explicit error. `httparse` is the only dep we'd consider pulling, and
even that is avoidable with a few lines of careful slice work. The size
budget of the bare-metal demo binary is more important than the readability
gain of a third-party client at this stage.

If the Hardened milestone introduces TLS or chunked transfer, this
decision is revisited. At that point a workspace-level `reqwless` (or
`embedded-tls` + `reqwless`) becomes plausible — but that is a Hardened
ADR, not this one.

### On 1 MiB slots for the demo

The ESP32-C3 SuperMini ships with 4 MiB flash. The IDF default with
two OTA slots and no `factory` leaves about 4 MiB minus ~64 KiB for
bootloader and partition table — i.e. close to 1.93 MiB per slot in
the most-generous layout. The conservative 1 MiB is sufficient for a
demo binary even in debug mode with logging, and leaves space for
future per-image metadata (signatures, manifests) without forcing a
re-flash and re-erase of the entire partition table when Hardened lands.

We pay roughly 1.86 MiB of unused flash per device for this margin;
that cost is acceptable in a demo context and avoids a partition-table
migration later.

Dropping `factory` is intentional: it keeps the demo focused on A/B OTA
semantics and gives both stacks the same boot slot model from the first
flash. The cost is that factory reset support is not available in the MVP
layout; recovery remains serial flashing with `otadata` erased.

### On the 30 s health criterion

The criterion has to satisfy two competing pressures: it must be
permissive enough that a slow Wi-Fi association does not roll back a
working image, and strict enough that an image that boots, hangs, and
never associates does roll back.

30 seconds plus a Wi-Fi association is a baseline both stacks can hit:
the IDF Wi-Fi join completes in 3-8 seconds on a healthy network; the
bare-metal join is comparable. Adding a margin to 30 s absorbs DHCP
retries and one Wi-Fi reconnect cycle, while still triggering rollback
on a hard hang or a Wi-Fi-credential mismatch in the new image.

Richer criteria (a successful HTTP round-trip back to the OTA server,
an application self-test, a watchdog tickle pattern) are
demo-application concerns rather than library concerns. They belong in
the demo binary or in `ota-hardened-v1`, not in the MVP library API.

## Consequences

### Positive

- **Single workspace to navigate** — OTA, Wi-Fi, MQTT, LoRa, and
  ESP-NOW share one `Cargo.toml`, one `justfile`, one `deny.toml`, one
  CI configuration. Pin discipline is centralised.
- **HTTP scope stays honest** — the README's "out of scope" note
  remains true for general-purpose clients. Consumers know
  `rustyfarian-network` is connectivity and firmware-update plumbing, not a
  reusable HTTP protocol library.
- **No premature dependencies** — the workspace does not adopt
  `reqwless`, `httparse`, or any other HTTP/TLS library at the MVP
  stage. Hardened can revisit with full information.
- **Demo and library boundaries are clean** — partition sizing and
  health criteria live in the demo and this ADR, not in the library
  API. The library stays policy-free.

### Negative

- **`rustyfarian-network` scope grows** — the workspace now also covers
  OTA, which is firmware-update plumbing rather than networking. The
  vision and README must be updated to match. This is the deliberate
  cost of avoiding a separate repo.
- **Hand-rolled HTTP carries some duplication risk** — if a future
  feature in this workspace also needs HTTP, the temptation will be to
  extract the OTA client. That extraction is acceptable when there is a
  second consumer; until then, the client stays internal.
- **The 1 MiB slot size leaves ~1.86 MiB of flash unused** on each
  ESP32-C3 demo device. Acceptable in a demo context.

### Implications

- The demo's `docs/features/ota-mvp-v1.md` open questions on partition
  layout and Wi-Fi credentials are now closed by this ADR for the MVP.
  The feature doc is updated to point at this ADR.
- The HTTP client implementation in `rustyfarian-esp-hal-ota` is marked
  with module-level documentation explaining it is not part of the
  crate's public API and may be removed if a workspace HTTP dependency
  arrives later.
- The bare-metal HTTP parser gets host tests for malformed status lines,
  redirects, duplicate or missing `Content-Length`, chunked transfer,
  oversized bodies, short reads, and successful fixed-length streaming.
- The demo flash recipe enables rollback, uses the shared partition table,
  erases or initializes `otadata` on first flash, and verifies the baseline
  image is marked valid before rollback tests.
- Hardened milestone explicitly inherits these decisions as starting
  points but is free to revisit any of them with documented reasoning.

## References

- [`rustyfarian-ferriswheel-demo` — `docs/features/ota-mvp-v1.md`](https://github.com/datenkollektiv/rustyfarian-ferriswheel-demo/blob/main/docs/features/ota-mvp-v1.md)
- [`rustyfarian-ferriswheel-demo` — `docs/project-lore.md`](https://github.com/datenkollektiv/rustyfarian-ferriswheel-demo/blob/main/docs/project-lore.md) — esp-hal pin discipline
- ADR 005 (this project) — Crate naming convention for dual-HAL drivers
- `esp-bootloader-esp-idf` 0.4.0 source — `OtaUpdater`, `set_current_ota_state`
- `esp-idf-svc` 0.52 source — `EspOta::mark_running_slot_valid`
