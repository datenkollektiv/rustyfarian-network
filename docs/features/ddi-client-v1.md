# Feature: DDI OTA client v1

The fleet backend chose a profile of the Eclipse hawkBit DDI API v1 as its device-facing OTA wire.
The device polls over HTTP with a per-device `Authorization: TargetToken <token>` header, and an optional MQTT "poll now" nudge wakes it early.
The profile is the fleet DDI profile (draft 0.1.0), and it is not frozen.
The maintainer reviewed it on 2026-11-20 and decided the items in this document.
This is the single feature doc for the work, and it is transient: it is archived when the feature closes.

## Problem

Today the library's OTA consumer runtime speaks the MQTT wire (ADR 018), and both tiers download with internal HTTP GET clients that are not a public API (the hand-rolled esp-hal client of ADR 011 decision 2, `EspHttpConnection` on ESP-IDF).
The fleet needs a device-facing HTTP polling client with TLS, origin-bound credentials and fail-closed update rules.
Neither tier has that today: the ESP-IDF tier has a downloader but no DDI transport, and the esp-hal tier has no TLS, no DNS on the OTA path and no OTA runtime.
This first round targets the ESP-IDF tier; esp-hal follows in a later round.

## Decisions

Decisions are grouped into themed tables, and each one was decided by the maintainer on 2026-11-20.

### Scope and ownership

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| A sans-IO DDI state machine in `juggler`, plus a credential-bound DDI transport per tier (ESP-IDF in this round) | Host-tested logic, with time and random values as inputs as `DownloadMachine` does (`crates/juggler/src/ota/download.rs:1-6`) | Generic GET, POST and PUT primitives with caller-supplied headers |
| No generic HTTP API is exported | `README.md:35-37` keeps general-purpose HTTP clients out of scope, and binding the token to the configured origin removes the leak path | Exporting a reusable HTTP client |
| ESP-IDF only in the first round; the esp-hal gap is recorded here (ADR 022): no TLS stack, IPv4 literals only on the OTA path (`crates/rustyfarian-esp-hal-network/src/ota/manager.rs:646-657`), a 256 B request buffer (`crates/rustyfarian-esp-hal-network/src/ota/http.rs:395`), no OTA runtime | The fleet devices run ESP-IDF, and the esp-hal gap is a separate body of work; the juggler machine is tier-neutral, so esp-hal reuses it later | Both tiers in the first release (decided first, revised the same day) |
| The MQTT OTA wire and runtime (ADR 018) move behind their own feature, are deprecated once the DDI client ships, and are then removed | DDI replaces it; two device-facing OTA wires would otherwise stay supported in parallel | Keep the MQTT wire long term |
| Feature split: `ota` keeps download, flash, record store and decisions; MQTT runtime and DDI runtime each get a feature; reporter retry pacing sits behind a transport-neutral seam | A DDI build must not link the MQTT command intake (`OtaSubmitter::handle` and `submit` are public today) | Keep one runtime feature |
| DDI client in 0.8.0 under the plain-HTTP lab opt-in; TLS in 0.8.0 or the next minor; not documented as ready outside a lab before TLS | Lets the fleet start on the wire while TLS lands | 0.8.0 waits for TLS; DDI in 0.9.0 |
| New ADR 027 for the DDI transport scope, superseding ADR 011 decision 2 for this purpose; ADR 018 amendment for the feature split and deprecation | Records the decisions where the project keeps architecture rules | Leave the scope in this feature doc only |

### Transport and security

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| HTTPS is the default and only release-image transport | Release devices must not accept plain HTTP | A runtime config flag |
| Plain `http://` only behind a cargo feature that release images leave out, with a WARN once per boot | The IDF downloader warns on every connection today (`crates/rustyfarian-esp-idf-network/src/ota/downloader.rs:207`) | A provisioned extra; anyone who reaches the portal could flip it |
| One CA compiled into the firmware, hostname verification, TLS 1.2 or later, never a fallback to plain HTTP | The CA must not be writable from the portal | CA provisioned into NVS (plaintext, portal-writable); a public CA bundle |
| Wall clock: TLS connects are refused until the clock reaches a build-time floor, with a distinct fail-closed error; the app starts SNTP | Certificate validity needs a sane clock | The library starts SNTP; the clock left entirely to the app |
| Origin binding: the transport takes base URL and token once and builds every request itself | No caller-supplied URL can carry the token elsewhere | Following server URLs unchecked |
| Before following any `_links` href or artifact URL: exact scheme, host and port match with the base | Stops a hostile or mistaken link from moving the token | Accepting any URL the server sends |
| Mismatch aborts with code 1006 | Gives the fleet a stable code for a rejected link | Silent skip |
| Redirects are never followed; a 3xx is a failure; the ESP-IDF transport sets `FollowRedirectsPolicy::FollowNone` | esp-idf-svc's default `FollowGetHead` follows a GET redirect and keeps every header set on the handle, so a token header would reach the redirected host | Follow redirects |
| POST and PUT always send `Content-Length` | Without it esp-idf-svc sends a chunked POST | Chunked bodies |
| JSON bodies bounded (4 KiB request, 8 KiB response) and buffered on the heap | Keeps worker stack use predictable | Stack buffers |
| `mqtts://` with the same compiled-in CA for the nudge connection | One trust anchor for both connections | Plain MQTT for the nudge |
| Token and `Authorization` value live in a `Secret` newtype with a redacting `Debug` | Keeps the token out of logs and debug output, following the library's existing redaction pattern | Plain `String` fields |
| `url_for_log` strips userinfo, query and fragment | The query can carry secrets; this is a public behaviour change | Strip userinfo only |
| Server-supplied strings that are logged have control characters escaped | Stops log injection | Log as received |
| Release images keep `CONFIG_LOG_MAXIMUM_LEVEL` at INFO or below and leave `CONFIG_MBEDTLS_DEBUG` off | ESP-IDF v6.1 `esp_http_client` prints the whole request header block, `Authorization` included, at DEBUG | Trust the default log level |

### Provisioning and identity

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| Device id rule `^[a-z0-9][a-z0-9-]{2,19}$`, equal to the DDI controller id, MQTT username and MQTT client id | One identity for all three uses | Separate names per service |
| Portal validator for the device id, on by default when DDI is enabled; prefills `mqtt_user` from the device name and leaves `mqtt_client` blank | Today the portal checks only presence, emptiness, duplicates and length (`crates/juggler/src/provisioning/form.rs:905-933`) | Presence-only check |
| A `MacSuffixed` prefix longer than 15 bytes or outside `[a-z0-9-]` is refused, not truncated | Silent truncation changes the identity (`crates/juggler/src/provisioning/device_name.rs:39-53` cuts to 19 bytes today) | Keep truncating |
| Token is the provisioning extra with form key `ddi_token`, stored in NVS as `x_ddi_token` | The 13-byte extra-name limit excludes the `x_` prefix (`crates/juggler/src/provisioning/config.rs:22-26`) | A dedicated portal field |
| Token validated as `[0-9a-f]{32,64}` at provisioning and before every use | CR or LF can never reach a header | Validate only at provisioning |
| Optional library-rendered secret token input in the portal: never prefilled, never echoed into HTML or logs | The token is not shown back to the operator | Prefilled input |
| `EXTRA_VALUE_MAX_LEN` (64) never lowered below 64 within 0.x | The token must fit | Lower the limit |
| Storage stays plaintext NVS (`crates/juggler/src/provisioning/kv_store.rs:11-14`) | NVS encryption is device-repo work | Encrypt in this library |
| Zeroing on drop covers the library's own copies only | The esp-idf-svc `CString` and the `esp_http_client` header list are freed unzeroed; one client per request keeps them short-lived | Promise zeroing of every copy |

### Update behaviour

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| `config.polling.sleep` clamped to [30 s, 24 h] as fixed library constants; same bounds for `Retry-After` and back-off | Bounded polling load and bounded waits | Caller-chosen bounds |
| `update: skip` means "not now": no download, action stays open, poll again, no feedback | The server decides the action; the device defers | Report skip as a failure |
| An artifact without `hashes.sha256` is refused | sha256 is the only corruption check | Accept unhashed artifacts |
| Artifact `size` compared with `Content-Length` and the bytes written; a `size` above the update partition is refused with 1006 before download | Catches truncation and oversize before any flash write | Check only the hash |
| Software-module metadata `chip=<TARGET_CHIP>` is required; missing or mismatched refused as `target_mismatch` | Stops a wrong-chip image | Trust the module name |
| `Version::parse` tightened to plain decimal `MAJOR.MINOR.PATCH`, no sign, no leading zeros, each at most 65535 | `u16::from_str` accepts `+1.02.3` today (`crates/juggler/src/ota/version.rs:40-74`) | Keep the lenient parser |
| Downgrade needs metadata `downgrade=allowed`; without it refused with 1007 and details `downgrade` | Keeps a device-side default against an accidental downgrade; the marker is unsigned metadata, so it is no defence against a hostile server | The server decides any version other than the running one |
| A remembered refused version stays refused unless the marker is present; the refused-version check runs before the downgrade branch | Keeps a refusal sticky | Forget refusals |
| Blocked offers (image pending verification, unresolved attempt, undelivered report) are not retrieved yet | No deploymentBase GET, so the server's silent timeout never starts and nothing counts against the rollout | Retrieve and queue |
| Polling and error statuses: 410 stop; 404 re-poll; 401 as in the feedback table, never with another credential; 413 and 429 honour `Retry-After`; 429 without it, 5xx and timeouts back off exponentially from the poll interval up to 24 h with jitter | Matches the fleet profile's status contract without guessing | Retry every status the same way |
| A transient error becomes terminal 1001 only inside an action once the download deadline passes, never on the poll path | A poll failure must not end an action | Fail on the first error |
| Cancel: the download machine checks a cancel flag after each read and at `Deadline::permit_activation` (`crates/juggler/src/ota/deadline.rs:56`); acknowledged until activation, rejected after it | Gives the operator an emergency stop for the whole download | Reject every cancel while an attempt is open |

### Feedback, nudge and conformance

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| Loop guard: DDI action id and stage persisted in the existing `ota` NVS namespace as `ddi_act` (str, 1 to 19 ASCII digits) and `ddi_stage` (u8); a closed action id is never re-applied | Stops a closed action from running again after reboot | Keep the action in RAM |
| Corruption fails closed; the DDI runtime calls `OtaStore::repair_corrupt` at boot when corruption closes admission, and reports `closed` + `failure` + 1005 + details `records_unreadable` | Corruption cannot wedge the device silently | Stay closed without a report |
| Terminal feedback (success and failure) persisted and retried until the server answers 200 or 410, no faster than 30 s | Today only `rolled_back` is persisted; `applied` is best-effort | Best-effort feedback |
| After 3 consecutive 401s the device stops retrying on that action, keeps the verdict, logs once, resumes after the next poll that is not a 401 | A bad token must not hammer the server | Retry forever |
| Health policy persists the success verdict and signals `ota-worker`, which sends the POST | Keeps the report off the health path | Send from the health check |
| Thread stack defaults chosen after measuring an mbedTLS handshake with the runtime's `StackMarks` on C3 and C6 | Measured, not guessed | Fixed guess |
| Feedback sequence: `proceeding` before the download and before the reboot; `closed` + `success` only after the image is marked valid | Matches the fleet's status points | Single final report |
| Rollback reported as `closed` + `failure` + 1004 + details `rolled_back` and the rollback reason; the old image polls at boot | The report beats the server's silent timeout | Report on the next normal boot |
| `details[0]` carries the `FailReason` string; code corrections `version_invalid` to 1005, `download_failed` with status 0 to 1001, `insufficient_space` to 1003 | Stable codes for the fleet | Keep the old mapping |
| Bounded feedback encoder: at most 10 details of 256 characters | Bounded heap and wire size | Unbounded details |
| Nudge: exact topic `rustyfarian/ota/<device-id>/nudge` registered with `.subscribe()` (re-sent on every connect) on the app's single MQTT connection | Survives reconnects | Subscribe once |
| Nudge polls at most once per 10 s and never while a poll is in flight, enforced in the juggler machine | Bounds the poll rate | Trust the server |
| `configData` is an optional call, once per boot, `mode: merge`, keys `chip`, `board` (app-supplied), `version` | Gives the fleet the hardware facts it asks for | Skip the call |
| Conformance: the fleet's Python harness runs in the fleet's CI against a pinned Rust host binary from this repo; this repo keeps a host-test copy of the scenario list | The fleet tests the real binary, and this repo catches drift | Fleet-only tests |

## Constraints

- The fleet DDI profile is draft 0.1.0 and not frozen; decisions here may follow its changes.
- Tier parity (ADR 022): the same API name, arguments and return type on both tiers, with only the error type differing; the esp-hal gap of this round is recorded in the Scope table, and the esp-hal round must land the ESP-IDF API unchanged.
- No reusable, exported HTTP client (`README.md:35-37`).
- Image signature verification is out of scope (device-repo work); sha256 detects corruption only.
- Residual clock risk: unauthenticated SNTP can still be pushed back to any time after the build date, so the build-time floor is not a hard guarantee.
- Storage of the token stays plaintext NVS until the device repo adds encryption.
- Release images must not raise `CONFIG_LOG_MAXIMUM_LEVEL` above INFO or enable `CONFIG_MBEDTLS_DEBUG`.

## Open Questions

- [ ] esp-hal round: TLS stack choice, DNS on the OTA path, and when it starts.
- [ ] Heap headroom with one HTTPS and one MQTTS session on C3 and C6 (hardware measurement).
- [ ] Worker and reporter stack defaults under TLS (after measurement).
- [ ] Name and wire code of the clock-not-set error.
- [ ] Fleet operator's confirmation of the downgrade marker rule (`downgrade=allowed`).
- [ ] Host-binary interface for the conformance harness (stdio or local HTTP).

<details><summary>Code references</summary>

- `crates/juggler/src/ota/download.rs:1-6`: sans-IO pattern the DDI machine follows.
- `crates/juggler/src/ota/download.rs:258-279`: Content-Length enforcement against bytes received.
- `crates/juggler/src/ota/error.rs:9`: `OtaError`, not `#[non_exhaustive]`, so the new clock variant is breaking.
- `crates/juggler/src/ota/persist/store.rs:917`: `OtaStore::repair_corrupt`.
- `crates/juggler/src/mqtt/mod.rs:54`: `format_broker_url`, where the scheme is chosen.
- `crates/juggler/src/provisioning/form.rs:87-94`: `ExtraField`'s redacting `Debug`, the pattern `Secret` follows.
- `crates/juggler/src/ota/runtime/report.rs:1-10` (transport-neutral pacing decisions): the reporter seam.
- `crates/rustyfarian-esp-idf-network/src/ota/runtime/intake.rs`: `OtaSubmitter`, which lives in the IDF crate.
- `crates/rustyfarian-esp-idf-network/src/ota/downloader.rs:63`: `url_for_log`.
- `crates/rustyfarian-esp-hal-network/src/ota/http.rs:395`: the 256 B request buffer.
- `crates/rustyfarian-esp-hal-network/src/ota/mod.rs`: `TARGET_CHIP` per chip.

</details>

## State

- [ ] ADR 027 written
- [ ] ADR 018 amendment (feature split and deprecation)
- [ ] Feature split (`ota`, MQTT runtime feature, DDI runtime feature, transport-neutral reporter seam)
- [ ] `juggler` DDI state machine
- [ ] `Version::parse` tightening
- [ ] `url_for_log` change and once-per-boot WARN on the IDF tier
- [ ] Provisioning validator for the device id and `ddi_token` input
- [ ] Cancel input on the download machine
- [ ] Persisted terminal verdicts and loop guard
- [ ] IDF DDI transport
- [ ] MQTT OTA wire and runtime deprecated when the DDI client ships, then removed
- [ ] Host tests and the conformance scenario list
- [ ] Hardware tests on C3 and C6 against a lab DDI server
- [ ] CHANGELOG entries (including the `url_for_log` and `OtaError` breaking changes)
- [ ] Documentation updated

## Session Log

- 2026-11-20 — Feature doc created from the maintainer's review of the fleet DDI profile draft 0.1.0.
- 2026-11-20 — Scope revised: ESP-IDF only in the first round (esp-hal gap recorded); the MQTT OTA wire is removed after its deprecation.
