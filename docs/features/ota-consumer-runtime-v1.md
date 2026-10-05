# Feature: OTA Consumer Runtime v1

Requested by `rustyfarian-rgb-clock` (first consumer) and Watchtower v2 (second consumer).
The decision core (`decide_offer`, `reconcile`, `OtaError::code`) ships unreleased on `main`.
Goal: lift the generic download hardening, wire contract, record store, and runtime into the library so both consumers reuse it.

**Status: All sub-projects (A–D) implemented, 0.6.0 pending hardware validation.**

## Decomposition

Four sub-projects, each gets its own spec → plan → implementation cycle, in order A → B → C → D.
B and C are independent and may proceed in parallel.
D depends on B and C.

| Sub-project               | Scope                                                                                                                                      | Tier(s)                           |
|:--------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------|
| **A. Download hardening** | Total deadline, `DownloadTimeout` reachable on IDF, `url_for_log` public                                                                   | Both                              |
| **B. Pure wire contract** | Manifest and command parsing, status JSON, reason codes; the HTTP manifest fetch belongs to sub-project D                                  | `juggler` (no MQTT/IDF tie-in)    |
| **C. IDF record store**   | Persisted `AttemptRecord`, rollback report, refused version in NVS; boot-facts reader (running slot, running state, update slot and state) | IDF tier                          |
| **D. Worker runtime**     | Background task loop, MQTT callback → channel handoff, health hook, rollback retries, manifest fetch over HTTP                             | IDF tier + `mqtt`, `ota` features |

## Sub-project A: Download hardening

A **cooperative deadline** is a total time limit for the entire download, checked at operation boundaries.

The deadline is expired when `elapsed >= total`.
`with_deadline(Duration::ZERO)` fails immediately with `DownloadTimeout` before any network or flash access.
A timeout never selects the new boot slot; finalization requires an `ActivationPermit` value obtainable only from a successful pre-finalization deadline check.

On IDF each network read is bounded only by the per-read timeout fixed at connection creation, so the worst-case overrun is one network phase or one flash operation.
On HAL each network wait is clipped to `min(per-read timeout, remaining deadline)` via `with_timeout`, so network waits never overrun; flash operations still can.

**Checked at:** operation start; after connect and headers; after partition preparation; after every chunk read and every flash write; after SHA-256 verification; immediately before finalization.

**Error mapping**

| Condition                                     | IDF before          | HAL before        | After A             |
|:----------------------------------------------|:--------------------|:------------------|:--------------------|
| Total deadline exceeded                       | —                   | —                 | `DownloadTimeout`   |
| Per-read timeout (IDF: `ESP_ERR_HTTP_EAGAIN`) | `ServerUnreachable` | `DownloadTimeout` | `DownloadTimeout`   |
| Connection reset / other read error           | `ServerUnreachable` | `DownloadTimeout` | `ServerUnreachable` |

Make `url_for_log` public in IDF `ota` module.
Both the HAL reset row and the IDF per-read timeout row change `OtaError::code()` output; each gets a CHANGELOG "Changed" line.

## Sub-project B: Pure wire contract

The wire contract is the JSON format on which MQTT commands and status messages travel.

**Why serde_json:** The rgb-clock already parses commands and status with `serde_json`.
Reusing the same derive stack ensures byte-compatibility: the serde error-handling, format rules, and escaping are identical by construction.
Both crates already link `alloc` (the ESP32 firmware includes it), so the `default-features = false, features = ["alloc"]` build of `serde` and `serde_json` fits without overhead.

Both tiers gain an opt-in `ota-wire` feature (`juggler` feature `ota-wire = ["ota", "dep:serde", "dep:serde_json"]`; IDF `ota` enables `juggler/ota-wire`; HAL exposes opt-in `ota-wire = ["ota", "juggler/ota-wire"]` requiring `alloc`).

**Types**

`FailReason` enum with `as_str(&self) -> &'static str` mapping to 17 frozen error codes.
`RollbackReason` enum with `as_str(&self) -> &'static str` mapping to 6 labels.
`OtaCommand` enum with variants `Update { manifest_url: String, sig_present: bool }` and `Rollback { from: Version }`.
`Manifest` struct with `version`, `sha256`, `url`, `target` fields (all `String`, all required, `deny_unknown_fields`).
`OtaStatus<'a>` enum with variants `Downloading`, `SwapPending`, `Applied { version: &'a str }`, `Failed { reason: FailReason }`, `RolledBack { reason: RollbackReason, attempt_id: u32, epoch: u32 }`.
`TARGET_CHIP: &str` constant in both tier `ota` modules, cfg-gated by target.

**Behaviour rules**

Command size ≤ 512 bytes, manifest size ≤ 1024 bytes (tested at exact boundary).
Command parsing rejects invalid UTF-8, invalid JSON, non-object top level, unknown fields, and duplicate keys as `Malformed`.
Target matching is exact and case-sensitive.
Manifest requires all four fields; missing, extra, null, or non-string values are `Malformed`.
Status JSON is compact with fields in order: tag first, then `reason`, `attempt_id`, `epoch`.
Errors hold no payload text; `serde_json::Error` is reduced to `Malformed { line, column }` (numbers only).

**Consumer review outcomes:** No wire byte changes to any reason strings or rollback labels.
`OtaStatus::Failed` and `OtaStatus::RolledBack` now carry typed reason enums instead of strings, serializing to the same JSON wire format.
JSON-array payloads (positional form) are now rejected as `Malformed`.

## Sub-project C: Record store and boot reconciliation

The record store persists `AttemptRecord`, rollback report, and refused version to NVS, with generic boot-reconciliation logic in juggler.

**Storage API decision**

The IDF crate cannot run host tests, so the per-key write order (the crash-consistency guarantee) must live where it can be crash-injected.
A key-level trait `OtaKv` in juggler (two implementors: `EspNvsKv` production + `FaultKv` test injection) enables this.
All record semantics, write ordering, and boot orchestration live as generic `OtaStore<K: OtaKv>` in juggler under the `ota-wire` feature.

**NVS layout and schema**

Namespace "ota", 16 keys (no schema key):

| Key | Type | Meaning |
|-----|------|---------|
| rb | u8 | 1 while report awaits broker ack, 0 once delivered |
| rb_why | str | reason of report, kept after delivery |
| rb_id | u32 | attempt id of report, kept until attempt cleared |
| att_ctr | u32 | attempt id counter, never removed |
| att_epoch | u32 | install epoch, random, created once, never removed |
| att_id | u32 | id of last attempt |
| att_ver | str | version of last attempt |
| att_slot | str | partition label attempt was written to |
| att_boot | u8 | 1 once boot slot switched |
| att_act | u8 | 1 once image is known activated |
| att_inv | u8 | 1 when target slot was already Invalid |
| att_why | str | why rollback started (first reason wins) |
| rq_id | u32 | event id for operator rollback without attempt |
| rq_why | str | reason of that operator rollback |
| rq_from | str | slot label rollback leaves (arm marker) |
| rej_ver | str | version last left by rollback, refused until a different version is successfully validated |

Attempt exists iff `att_ver` AND `att_slot` present; request exists iff `rq_from` present; report exists iff `rb` present.

**Write sequences**

Rule: one commit per key operation, listed order IS the crash order.
Every string `set` = erase commit then set commit; a failed set leaves the key ABSENT.

**S0 next_attempt_id:** `[set att_epoch = entropy() if absent]`; `set att_ctr = ctr.unwrap_or(0).wrapping_add(1)`, skipping 0.

**S1 begin_attempt:** precheck `admission()==Open` else `NotAdmitted`.
S0.
`rm att_why; rm att_slot; rm att_ver; rm att_act; rm att_boot; rm att_id; set att_id; set att_inv; set att_ver; set att_slot` (commit marker).

**S2 mark_boot_selected:** `set att_boot=1`.
**mark_activated:** `set att_act=1`.

**S3 report_rollback:** Guard first (`rb==1 and rb_id Some(other) → ReportPending`, no write).
`rm rb_id; set rb_why; set rb=1; set rb_id=attempt_id`.
If `left is Some`: `set rej_ver`.
Then clear_attempt.

**S4 complete_attempt:** Read `rej_ver`.
Absent → skip; unparseable or differs from `running` → `rm rej_ver`; equal → keep.
No `set rej_ver` writes between S3 and S5.

**S5 clear_attempt:** `rm att_ver; rm att_slot; rm att_act; rm att_boot; rm att_inv; rm att_id; rm att_why`.

**S6 note_rollback:** Read attempt.
If Some and slot==running: `att_why` present → `KeptFirst` else `set att_why → Written`.
Otherwise: `rq_from` present → `KeptFirst`; else S0; `set rq_id; set rq_why; set rq_from` (arm last) → `Written`.

**S7 settle_request:** Read request.
None → `NoRequest`; missing id is `Corrupt(RequestWithoutId)`.
from==running → `remove_request` → `DroppedNotRolledBack`.
Else if `rb==1 and rb_id==Some(rq_id)` → `remove_request` → `AlreadyReported`.
Else if `report_undelivered` for another id → `remove_request` → `MergedIntoPending`.
Else `publish(rq_id, rq_why)` then `remove_request` → `Reported`.

**S8 adoption:** With no attempt, never assign a fresh id to `rb_id`.

**S9 refuse_version:** Read `rej_ver`.
Equal → `AlreadyRefused`; else `set rej_ver → Written`.
Restore: atomic replace, or accept loss if `set` is erase-then-set.

**S10 reconcile_boot:** Phase 1 (read-only): `hw` must be `Ok` (else `FailedClosed(Hardware)`, nothing written).
Read attempt, request, report, refused.
Any Err or Corrupt → `FailedClosed(Store)`, nothing written.
Phase 2: build `BootFacts` and call core `reconcile`, then dispatch outcomes.

**Recovery table**

| Sequence | Outcome |
|----------|---------|
| S0 crash after epoch | ctr absent treated as 0, next call writes 1 |
| S1 crash after removals | no attempt; hardware shows old image → reconcile ClearAttempt |
| S1 after att_slot | attempt exists, boot not switched; hardware shows old → reconcile ClearAttempt |
| S3 after rm rb_id/rb_why | report absent/stale; P repeats |
| S3 after rb=1 before rb_id | adoption adopts rb_id=attempt_id |
| S7 after P keys before remove | AlreadyReported then remove |
| Corrupt slot label | FailedClosed; repair_corrupt is explicit exit |
| Hardware Busy/Read | FailedClosed; D retries later or never marks valid |
| update_slot failure | soft (None); no impact |

**Boot facts reader**

IDF `read_hardware_facts(BusyRetry)`: each try creates short-lived `EspOta` handle to get running and update slots.
`ESP_ERR_INVALID_STATE` is Busy; acquisition or running-slot failure sleeps and retries.
Running label `ota_N` → `SlotId(N)`; `factory` or other non-ota label → `FACTORY_SLOT` (0xFF), not an error.
Update slot failure is soft (None).

**Outcome for D**

`slot_released` = hardware read succeeded AND running slot is not `PendingVerify`.
`admission_open_at_boot` = boot-time snapshot = `slot_released && store.admission()==Open && !failed_closed`.
It goes stale as soon as a report is persisted or an attempt starts; D must gate every offer on a live `store.admission()` call.
`refuse_mark_valid` = `FailedClosed` or `RollBackNow`.
`roll_back_now` = `RollBackNow` only.
`report_pending` = `store.report_undelivered()`.

**Rejected alternatives**

- Concrete-only store in IDF crate: untestable on host.
- Record-level trait: pushes per-key order into untested IDF code.
- Reusing ota_lifecycle Nvs struct: hides id-reservation windows.
- Changing key layout: clock must adopt without migration.
- Gating schema check: additive layout plus new namespace is simpler.
- Holding EspOta in store: process singleton would starve FirmwareFlasher.
- Settling request in same boot as ReportRollback: single-report guard would merge and lose reason.

## Sub-project D: Runtime and health policy

The runtime owns the background worker and reporter threads, manifest fetch over HTTP, health-policy loop and reconciliation on the app's main thread, and the handoff between MQTT callbacks and the worker via a bounded channel.
The library commits to never calling `esp_restart` or switching boot slots outside the existing `OtaSession::rollback` and `mark_valid` paths; the app supplies a restart callback.

**Hook points and wiring**

1. Link patches, logger, take Peripherals/sys_loop/nvs; **`let boot = Instant::now();` immediately.**
2. HOOK 1: `let records = open_records(nvs.clone());` before WS2812/provisioning/Wi-Fi.
3. Wi-Fi up (`get_ip`).
4. HOOK 2: `channel(OtaConfig::new(..)?)` before the MQTT builder.
5. `MqttBuilder::new(cfg).subscribe(submitter.command_topic(), AtLeastOnce).on_message(move |t, d| { if submitter.handle(t, d) return; .. }).build()?` — callback only parses, atomics, try_send.
6. HOOK 3: `runtime.start(mqtt.clone(), records?)?` — boot reconcile on caller thread (roll_back_now outside any closure), flags set, reporter spawned, then worker.
7. HOOK 4: `ota.run_health_policy(boot, || tick_fresh() && mqtt.is_connected() && ipv4())` on main, then park.

Flag initial state at `start()`: `admission_open` and `health_settled` both start as `initial_admission(&outcome)`.

**Threads and channels**

Threads via `std::thread::Builder`: `ota-reporter` 8192 B, `ota-worker` 16384 B; floor 4096 B; health on app main thread.
Channels: command `sync_channel::<OtaCommand>(1)`; reporter queue `sync_channel::<ReporterMsg>(reject_queue_depth=4)`.
Shared atomics: `busy` (CAS false→true AcqRel), `closed` (set on start failure or unstarted drop), `rejects_total` (monotonic u32).

**Worker flow**

Loop: (a) retained offer + `flags.admission_open` + `records.with(admission)==Open` + CAS wins → execute silent retry.
(b) else recv/recv_timeout; only Update replaces retained; Repair/Rollback untouched.
Gate: `flags.admission_open` and `running_state!=PendingVerify`.
Update: fetch manifest (err → reason), check target, decide offer, begin attempt, OtaSession + deadline, fetch_and_apply, mark_boot_selected, swap_pending, restart grace, call restart.
Rollback: hw read fail check, PendingVerify check, one lock `rollback_arm`, then `OtaSession::rollback`.
Repair: plan repair, loop until `Ok(None)`.

**Reporter flow**

Loop: `recv_timeout(machine.next_wait(now))`; wait always bounded.
When due + connected: publish pending report; Err keep & retry; Ack → `mark_report_delivered` only.
OtherReport: at most 2 passes then wait.

**Health policy**

`run_health_policy(&self, boot, healthy)` on main; guarded by swap, second call → `AlreadyRunning`.
S0: read slot every slot_retry; Busy never counts.
S1: not PendingVerify → `admission_open=true`, `NothingToVerify`.
S2: PendingVerify poll every poll_interval: healthy := `!refuse_mark_valid && elapsed≥healthy_dwell && predicate()`.
On healthy: `mark_valid` (Err keep polling); `complete_attempt` (3 retries); `Applied` or `ApplyNotRecorded` (warn, `applied` still published).
S3: `elapsed≥failure_deadline`: Rollback (3 attempts) or NoteAndRestart.

**Reboot contract**

Runtime never calls `esp_restart` or switches slots.
Restart only via `(cfg.restart)()` after swap_pending+restart_grace (worker) or on NoteAndRestart (health); if callback returns, library parks.
Only rollback inside `OtaSession::rollback` (operator, health deadline, boot roll_back_now).

**Behaviour changes versus the first consumer's runbook**

The runtime deliberately differs from the first consumer's hand-written layer in these points; the hardware tester should expect them.

1. Update, rollback and repair commands that arrive before the health policy has read the slot: on a boot whose running slot is already valid (not refused, not failed closed) they are accepted at once, as in the first consumer.
   On a boot whose running slot is pending verification, or cannot be read, the health policy is the only opener: an update is retained (first sight reported as `failed{pending_verify}`) and runs once admission opens, and a rollback or a repair answers `pending_verify`.
2. When `mark_valid` succeeds but `complete_attempt` fails all its retries (`ApplyNotRecorded`), `applied` is published immediately, as in the first consumer, and the verdict tells the app the attempt record was not cleared.
   The reporter thread keeps retrying `complete_attempt` for the running version with backoff (`complete_retry`, doubling up to `report_retry`) until it succeeds or the attempt is gone.
   Until then every later push answers `failed{attempt_unresolved}` and is retained; admission reopens by itself on success.
   `applied` is not republished at the next boot.
3. A deferred offer can appear on the wire as `rolled_back`, then `failed{report_pending}`, then `downloading`, because the reporter and the worker are separate threads.
4. A hardware read error at the update gate is reported as `busy` or `partition_not_found` before the manifest is fetched; the first consumer fetched first and treated a read error as no refusal.
5. An operator rollback on the factory image answers `rollback_unavailable`.
6. The retained offer survives rollback and repair commands; the first consumer cleared it on any command.
7. A busy slot read never counts toward the health give-up, so the policy can retry for as long as the handle stays busy; the first consumer gave up at the deadline.
8. An unreadable refused-version record (`rej_ver`) is logged and ignored, as in the first consumer; the next completed update heals it.
9. On a pending or unreadable slot, a repair command answers `pending_verify` until the health policy has settled, without touching the OTA handle.
10. The worker stack has its own floor (`MIN_WORKER_STACK_BYTES`, 12288) above the reporter floor (`MIN_STACK_BYTES`, 4096).
11. New wire vocabulary: command `repair`, status `repaired`, failure codes `repair_failed` and `repair_not_needed`.
    `create_http_connection` gained an `accept` parameter (public API break).

**Rejected alternatives**

- Keep rolled_back retry in worker: stalls behind 300 s download.
- Library calls esp_restart: breaks reboot contract.
- Runtime settle_request: races with this-boot rollback requests.
- Blocking publish from worker/health: can stall; bounded retry instead.

## Hardware Validation State

**Sub-project A:** Host-tested via juggler deadline tests + IDF error mapping; hardware check pending.

**Sub-projects B–D:** Host-tested via `just test-ota` (FaultKv crash-matrix, adoption and repair, write sequences, boot reconciliation).
Library runbook at `docs/runbooks/ota-hardware-test.md` documents the test matrix.
The clock's own hardware passes (2026-09-28) are evidence for the clock's code, not validation of the extracted runtime.
C and D are complete only when the clock has adopted the runtime and the runbook passes on the reference example and on both consumers, with a per-scenario result for each.
Pending: clock adoption on real NVS + full runbook + power-loss scenarios + Watchtower v2 validation.

## Open Questions

- Confirm the repair wire names: `{"action":"repair"}`, `repaired{records}`, `repair_failed`, `repair_not_needed`.
- Confirm that the manifest fetch keeps the HTTP client's default redirect policy (`FollowNone` deliberately not set).
- Whether `BlockedBy` needs a richer variant for unreadable records (today `Blocked(AttemptUnresolved)`).
- `MergedIntoPending` drops the request's reason; a second report slot may be needed later.

## Constraints

- A and B ship together with C and D in one `0.6.0` (decision 2026-10-06); nothing is tagged before that.
- Sub-projects B, C and D reverse decision-core decisions; ADR 018 records the reversal.
- Sub-project D must honour ADR 017: no client calls from MQTT callbacks; hand off via channel.
- Topics are parameters; the clock's `ota/command` and Watchtower's `rustyfarian/watchtower/<id>/ota/command` must both fit.
- Signature verification belongs to the security-model roadmap item; the `sig` field stays reserved.
- HTTPS is out of scope for v1.
- Fleet backend (hawkBit or similar) stays with the consumer.

## State

- [x] A–D design approved and implementation complete; host gates green.
- [x] ADR 018 ("OTA consumer runtime in the library") written.
- [x] Both consumers' preliminary review received (C/D API, hook wiring, clock adoption path).
- [ ] A hardware check (ESP32-C3 runbook).
- [ ] C + D hardware validation (clock adoption on real NVS, full runbook, power-loss scenarios).
- [ ] Watchtower v2 hardware validation on second device.
- [ ] `cargo semver-checks` recipe added and run against v0.5.0 before 0.6.0 release.

## Session Log

- 2026-10-05 — Feature doc created; A designed (cooperative deadline, boundaries, permit-gating, error mapping).
- 2026-10-05 — A implemented; B/C/D requirements drafted in details blocks.
- 2026-10-05 — B design approved: serde_json chosen; wire contract types to juggler modules.
- 2026-10-05 — B implemented; clock review: no byte changes to reason strings or rollback labels.
- 2026-10-06 — Watchtower v2 polish: URL validation, `Rollback.from` typed as `Version`, `TARGET_CHIP` constant, strict deserialization.
- 2026-10-06 — C design approved: key-level `OtaKv` trait in juggler, generic `OtaStore<K>` under `ota-wire`; NVS layout 16 keys.
- 2026-10-06 — C implemented; FaultKv crash-matrix, adopt_clock/self_heal/sequences/crash_matrix/fail_closed/note_boot_slot host tests green.
- 2026-10-06 — C consumer reviews: counter-loss fix, `Delivery`/`RollbackNoted`/`BootWarnings`, single-write rb repair.
- 2026-10-06 — D design approved: module layout, OtaConfig/OtaSubmitter/OtaRuntime/OtaHandle; health policy; reboot contract.
- 2026-10-06 — D implemented; `idf_c3_ota_runtime` example builds; runbook doc added.
- 2026-10-06 — Old layouts (ef7818b..a14f555: `rb_from`/`rb_conf`, missing ids) no longer supported; missing ids are corrupt, cleared by `repair_corrupt`.
