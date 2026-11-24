# ADR 027: DDI Transport Scope

## Status

Proposed (2026-11-20).
Supersedes [ADR 011](011-ota-crate-hosting-and-transport.md) decision 2 in part, for the DDI transport only.
Builds on [ADR 018](018-ota-consumer-runtime-in-library.md), [ADR 022](022-tier-parity-one-async-api.md) and [ADR 023](023-shared-logic-ladder.md).

## Context

The fleet backend uses the fleet DDI profile (draft 0.3.0), a profile of the Eclipse hawkBit DDI API v1.
A device polls over HTTP with a per-device `Authorization: TargetToken <token>` header.
The profile requires HTTPS, origin-bound credentials and fail-closed update rules.
The client decisions are recorded in [`docs/features/ddi-client-v1.md`](../features/ddi-client-v1.md), and this ADR records only the transport scope.

[ADR 011](011-ota-crate-hosting-and-transport.md) decision 2 keeps the HTTP transport internal and plain: a strict, hand-rolled HTTP/1.1 `GET` with no TLS.
Its Rationale defers TLS and chunked transfer to a "Hardened ADR".
The README keeps general-purpose application-layer HTTP clients out of scope.
The DDI client needs `POST` and `PUT`, request headers and TLS.
A generic client that carried these would contradict the README and open a path for the token to leave the configured server.

### Approaches evaluated

- **Generic GET, POST and PUT primitives with caller-supplied headers**: the caller could attach the token to any URL.
- **An exported reusable HTTP client**: it contradicts the README scope line and has the same leak path.
- **A single typed request enum with one `execute`**: it is a wide entry point that drifts towards a generic client.
- **Per-endpoint methods that return parsed juggler types**: the codec and status logic would move into the adapter, against the ADR 023 review rule.

## Decision

### Scope and ownership

- There is one credential-bound DDI transport per tier.
- The ESP-IDF transport ships in this round.
- The esp-hal transport follows in a later round, and its gaps are listed in the Scope table of the feature doc: no TLS stack, IPv4 literals only on the OTA path, a 256 B request buffer, no OTA runtime and none of the `ota::ddi` runtime types of this round (`DdiNudge` included). 2026-11-24: the esp-hal DDI client is implemented, and the URL pull API is removed on both tiers (`EspHalOtaManager::fetch_and_apply*` and `OtaSession::fetch_and_apply*`), so the gaps listed here for the pull path no longer apply.
- No generic or exported HTTP client is added.
- The transport is DDI-only: no public method takes a URL string, headers or a method, and the requests it can make are fixed.
- The sans-IO DDI machine lives in `juggler::ota::ddi` (ADR 023 rung 3), next to the codec in `juggler::ota::ddi::wire`.
- The transport only performs I/O and maps errors.
- Parsing (documents, status handling and the `Retry-After` value) and every decision stay in `juggler`, as the ADR 023 decision 7 review rule requires.
- This ADR supersedes ADR 011 decision 2 for the DDI transport only.
- The hand-rolled esp-hal GET client and the MQTT wire's manifest and firmware fetch (`FirmwareDownloader`, `create_http_connection`) keep ADR 011, plain HTTP, unchanged (2026-11-24: both were removed with the MQTT wire and the URL pull API).
- ADR 011 deferred TLS to a "Hardened ADR"; this ADR is that decision for the DDI transport only.

### Names and features

- The ESP-IDF module is `rustyfarian_esp_idf_network::ota::ddi`.
- `DdiTransport` is built from a `DdiTransportConfig`, which holds the base URL, the controller id and the token.
- The token is held in a `Secret` newtype with a redacting `Debug`.
- The same names are reserved for the esp-hal round.
- The cargo feature `ota-ddi = ["ota", "juggler/ota-ddi"]` is the DDI runtime feature.
- The feature split that creates it is the ADR 018 amendment and is not decided here.
- The cargo feature `ota-ddi-insecure-http` is the plain-HTTP lab opt-in, and release images leave it out.
- Without that feature, an `http://` base URL is refused when `DdiTransport` is constructed.
- With that feature, the transport logs a WARN once per boot, not once per connection as `downloader.rs` does today.

### API shape: one narrow method per DDI endpoint

ADR 023 decision 10 applies: each method is one endpoint with no decisions.

- `poll()` sends a GET to the controller base resource, with the URL built from the configured base and controller id.
- `deployment(&Href)` sends a GET to a server-supplied link.
- `cancel_action(&Href)` sends a GET to a server-supplied link.
- `deployment_feedback(ActionId, &Feedback)` sends a POST, with the URL built from the configured base and the action id, not from a link.
- `cancel_feedback(ActionId, &Feedback)` sends a POST, with the URL built from the configured base and the action id, not from a link.
- `config_data(&ConfigData)` sends a PUT, with the URL built from the configured base.
- `download(&mut OtaSession, &Href, sha256: &[u8; 32], size: u64, &OtaCancel) -> Result<(), OtaError>` fetches the artifact into the session.
- GET, POST and PUT return `DdiResponse { status: u16, retry_after: <raw header value, bounded>, body: Vec<u8> }`.
- The body is capped at 8 KiB on the heap (`MAX_RESPONSE_BYTES`), and an over-cap body is an error, never a truncated body.
- The transport exposes the status and does not interpret it.
- Request bodies are at most 4 KiB and are buffered on the heap.
- POST and PUT always send `Content-Length`, because esp-idf-svc otherwise sends a chunked body.

### Origin binding and redirects

- The transport takes the base URL and the token once and builds every request itself.
- Any `Href` it is handed (deployment, `cancelAction` or artifact) is checked by the transport against the configured origin, even though the juggler machine has checked it already.
- The origin check compares the exact scheme, host and port.
- The check fails closed with a distinct error before any request is made.
- The transport reports a foreign origin and a 3xx as distinct errors; the juggler machine maps them per the profile: 1006 (`details[0]` `foreign_origin` or `redirect`) for the artifact URL and for the deployment and download requests, no request and no feedback for a foreign poll link, and transient for a 3xx on the poll or a feedback request.
- The comparison function is pure and lives in `juggler`; the transport only calls it.
- Redirects are never followed: every ESP-IDF request sets `FollowRedirectsPolicy::FollowNone`, and a 3xx is a failure.
- esp-idf-svc's default `FollowGetHead` would follow a GET redirect and keep every header set on the handle, so the token would reach the redirected host.
- `download` reuses `OtaSession` and the existing `DownloadMachine` loop through a crate-private request path that carries the `Authorization: TargetToken` header and `FollowNone`.
- The public `OtaSession::fetch_and_apply*` methods keep their URL-only signature, and no public method gains a header parameter (2026-11-24: those public methods were removed, and the crate-private path stays).
- The size is compared with `Content-Length` and with the bytes written.
- Each request uses its own HTTP client, so the copies of the token made by esp-idf-svc (the `CString` and the `esp_http_client` header list) stay short-lived.
- Zeroing on drop covers the library's own copies only.

### TLS and security

- HTTPS is the default transport.
- The image carries exactly one trust anchor, fixed at image build as one CA certificate or one pinned server key.
- No provisioning or runtime input can change the trust anchor.
- Hostname verification and TLS 1.2 or later are required, and there is never a fallback to plain HTTP.
- TLS connects are refused until the wall clock reaches a build-time floor, with a distinct fail-closed error.
- The app starts SNTP.
- That clock error has no wire code of its own; its name is the library's choice when S2 lands (feature doc, profile 0.4.0 §1, §4).
- TLS lands in 0.8.0 or in the next minor release.
- Until TLS lands, the DDI client ships only under the plain-HTTP lab opt-in and is not documented as ready outside a lab.
- Release images keep `CONFIG_LOG_MAXIMUM_LEVEL` at INFO or below and leave `CONFIG_MBEDTLS_DEBUG` off.
- ESP-IDF v6.1 `esp_http_client` prints the whole request header block, `Authorization` included, at DEBUG.

### Execution model and tier parity (ADR 022)

- Tier parity applies to the public transport API.
- Both tiers use one type name per role and the same method names; argument and return types match except for the error type and the execution model (blocking or `async fn`), and the esp-hal round may still change how `download` gets its socket.
- The execution model is tier-native, and that difference is a named parity gap.
- The ESP-IDF `DdiTransport` is blocking and runs on the library-owned `ota-worker` thread, as `OtaSession` does.
- The esp-hal round lands the same names and arguments as `async fn`, as `EspHalOtaManager` does today.
- ADR 022 decision 10 puts the async-first parity surface on the runners' handles, which for DDI is the runtime handle in a later task.
- ADR 023 decision 9 keeps worker threads a tier fact.
- The ADR 022 open-question answer of 2026-11-14 rules out wrapping the transport in `block_on` inside the library.
- This gap departs from ADR 022 decisions 2 and 3 for the DDI transport, which ask for async wrappers over the worker threads and for new features to land async-first.
- An ESP-IDF async wrapper over the worker is deferred until a consumer asks for one.
- The blocking transport is the first round's proof of concept; the esp-hal round revisits the choice, together with how `download` gets its socket.
- The transport's public items carry the "Experimental: API may change before 1.0" marker, so that revision needs no deprecation cycle.

## Rationale

Narrow per-endpoint methods are the only shape where the caller cannot choose the URL, the header or the method.
Each method maps to one request, so the transport stays free of codec and status logic, and the juggler machine stays the only place that decides.

A followed redirect hands the token to a host the operator did not configure, so redirects are refused rather than checked after the fact.

Chunked request bodies are refused because the body is bounded and known before the request, so the transport always sends a length.

A runtime plain-HTTP flag, or a plain-HTTP setting in the provisioned extras, is refused because anyone who reaches the portal could flip it.

A CA certificate provisioned into NVS is refused because NVS is plaintext and writable from the portal.
A public CA bundle is refused because the fleet uses one trust anchor.

Async on the ESP-IDF tier now is refused, because each request would cost a thread hop and a channel, and the IDF runtime would have to `block_on` its own transport.
Blocking on both tiers is refused, because it breaks ADR 022 on the native async tier.

The ADR 011 reasons for keeping the MQTT wire's HTTP internal and plain still hold, and this ADR does not change them.

## Consequences

### Positive

- The token cannot leave the configured origin by construction: the caller supplies no URL, no header and no method, redirects are refused, and each request has its own client.
- The README scope line stays accurate.
- The DDI machine is host-tested in `juggler`.
- The esp-hal round has a fixed API to land.

### Negative

- Until the MQTT wire was removed on 2026-11-24, there were two download paths with different security: plain HTTP on the MQTT wire and HTTPS on the DDI client.
- The sync and async parity gap is recorded here, and it departs from ADR 022 decisions 2 and 3 for the DDI transport.
- The esp-hal `download` must also settle how the TCP socket is supplied, because the esp-hal precedent `EspHalOtaManager::fetch_and_apply_cancellable` takes a `TcpSocket` (2026-11-24: that precedent was removed with the esp-hal pull updater).
- The heap headroom for one HTTPS session and one MQTTS session on C3 and C6 is not measured yet (open question in the feature doc).
- The token copies made inside esp-idf-svc are freed but not zeroed.
- Unauthenticated SNTP can still move the clock back after the build-time floor, so the floor is not a hard guarantee.

### Neutral

- The feature split and the MQTT wire deprecation are separate decisions, recorded in the ADR 018 amendments of 2026-11-21 and 2026-11-24.

## References

- [ADR 011 — OTA Crate Hosting and Transport Decisions](011-ota-crate-hosting-and-transport.md)
- [ADR 018 — OTA Consumer Runtime in the Library](018-ota-consumer-runtime-in-library.md)
- [ADR 022 — Tier Parity, One Async API](022-tier-parity-one-async-api.md)
- [ADR 023 — Shared Tier Logic Follows a Three-Rung Ladder](023-shared-logic-ladder.md)
- [Feature: DDI OTA client v1](../features/ddi-client-v1.md)
- `crates/juggler/src/ota/ddi/wire/mod.rs` (the DDI codec and its size limits)
- `crates/rustyfarian-esp-idf-network/src/ota/downloader.rs` (`FirmwareDownloader`, `create_http_connection`, removed 2026-11-24)
- `crates/rustyfarian-esp-idf-network/src/ota/mod.rs` (`OtaSession::fetch_and_apply_cancellable`, removed 2026-11-24; `OtaCancel`)
- `crates/rustyfarian-esp-hal-network/src/ota/manager.rs` (`EspHalOtaManager::fetch_and_apply_cancellable`, removed 2026-11-24)
