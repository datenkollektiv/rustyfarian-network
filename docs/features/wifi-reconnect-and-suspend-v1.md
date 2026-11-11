---
gate: none
desk-work: available
---

# Feature: Wi-Fi reconnect and radio suspend/resume on both tiers v1

Request U2f from `rustyfarian-knob` (milestone M8, `review-queue/rustyfarian-network-u2f-reconnect-and-suspend-resume.md`).
The IDF `WiFiManager` connected once and only logged `StaDisconnected`, so a router outage left the station down until reboot.
On esp-hal every example carried its own copy of the reconnect loop, so a fix there had seven places to land.
Neither tier could take the radio down before deep sleep and bring it back in the same boot.

## Decisions
| Decision                                                                                                                  | Reason                                                                                                      | Rejected Alternative                                                    |
|:--------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------|
| One pure `juggler::wifi::LinkMachine` (inputs → actions) owns back-off, suspend and the connect-after-suspend race        | Host-tested, one place for this bug and the next; same shape as `ScanMachine` (ADR 020)                     | Per-tier loops that happen to agree                                     |
| Back-off 1 s doubling to 30 s via `juggler::backoff::ExponentialBackoff`, reset on `Connected`                            | Already tested; constants in `juggler::wifi` so both tiers read the same numbers                            | Fixed 500 ms / 5 s delays as in the old examples                        |
| IDF: a worker thread in `WiFiManager` executes `Connect` and `Wait`; the event subscription feeds the machine             | The system event loop must not sleep; the thread is the only writer of `esp_wifi_connect`                   | `esp_timer` one-shots from the handler                                  |
| IDF reconnect covers both connect modes, armed after the initial connect phase                                            | Blocking-mode firmware (`idf_c3_mqtt`) recovers too; no race with the blocking loop                         | `NonBlocking` only, as the request literally says                       |
| esp-hal: `wifi::WifiLink`, a `const` static the firmware spawns with `run(controller)`                                    | Library-owned loop; examples shrink to one spawn; opt-in keeps the handle API                               | A loop inside `init_async` (cannot: the controller is `&mut` in a task) |
| Suspend is a state of the running stack: IDF `disconnect`+`stop` / `start`+`connect`; hal `disconnect_async` and park     | `Modem` cannot be re-created; esp-radio has no stop; deep sleep must stay abortable                         | Drop and re-create the managers                                         |
| IDF `suspend()`/`resume()` run their IDF calls inline on the caller's thread                                              | Millisecond-bounded; the radio is stopped when `suspend()` returns, as deep sleep needs                     | Hand them to the worker (asynchronous, caller must poll)                |
| MQTT during suspend: no new API; the dying transport sends no DISCONNECT, the broker fires the LWT after 1.5 × keep-alive | `esp_mqtt_client_stop`/`destroy` send DISCONNECT and block; stopping Wi-Fi is the only LWT-preserving pause | `MqttHandle::suspend()` wrapping `esp_mqtt_client_stop`                 |
| hal `suspend()`/`resume()` set the state at once, `run` acts on a `Signal`                                                | state() is Suspended immediately; run reacts at every await (select)                                        | A command channel drained by run (state lags; queue can overflow)       |
| IDF worker drops link events behind a stop; `resume()` never waits             | Stale pre-stop events cannot start a second connect, however late they arrive | A resume-side timeout that clears the fence (late events slip through)  |
| IDF `try_publish*` returns `WouldBlock` while `is_connected()` is false, like the hal tier                                | One publish contract across tiers ("streamline the two solutions")                                          | Keep the esp-mqtt outbox behaviour and document the gap                 |
| Graceful variant is a documented recipe: `publish_acked` offline, then `suspend()`                                        | The firmware holds the handle and the topic; nothing for the library to add                                 | `suspend_graceful(topic, payload)`                                      |
| Twin example `idf_esp32s3_mqtt_lwt`, client id `idf-s3-mqtt`, prefix `rustyfarian/idf-mqtt`                               | Same env keys as `hal_esp32s3_mqtt_lwt`; distinguishable on one broker                                      | Reuse `idf_c3_mqtt` on the S3                                           |
| `MQTT_SUSPEND_DEMO=1` suspends 20 s every 60 s in both S3 examples                                                        | Same trigger on both tiers for the knob's protocol                                                          | An MQTT command to suspend (needs the link that is being suspended)     |

## Constraints
- Additive API except the IDF `try_publish*` gate; `AsyncWifiHandle`, `init_async` and the IDF constructors keep their signatures.
- ADR 021 holds: the library selects no console, panic handler, critical-section provider or scheduler.
- `suspend()` and `resume()` are idempotent and bounded (IDF: milliseconds; hal: non-blocking signal).
- Auto-reconnect is paused while suspended on both tiers; a `StaDisconnected` caused by `suspend()` never triggers a connect.
- A publish in flight around suspend may be lost; delivery is best effort (accepted by the requester).
- Deep sleep: IDF calls `suspend()` (which runs `esp_wifi_stop`) before `esp_deep_sleep_start`; esp-hal needs only `suspend()`, the reset re-initialises the radio.
- A suspend shorter than 1.5 × keep-alive ends with a client-id takeover on resume: the broker publishes the will, then the startup message; the 3-minute outage fires the will the normal way.
- `just fmt`, `just verify`, `just test` and `just build-example` for all 7 converted hal examples plus the two S3 LWT examples stay green.
- The aborted-sleep case is covered: a `resume()` within 50 ms of `suspend()` leaves exactly one connect in flight.

## Open Questions
- [ ] Add jitter to the back-off once a fleet of devices behind one AP needs it (deterministic delays resynchronise after an AP outage)?
- [x] esp-mqtt's 10 s reconnect churn while Wi-Fi is stopped is documented as acceptable log noise; raise `reconnect_timeout` to quiet it, at the cost of a slower reconnect after resume.
- [ ] Add a watchdog for a `Connecting` or fenced link that never sees its event, should a tier ever lose driver events (today both rely on in-order delivery)?

## State
- [x] Design approved
- [x] PR 1 (U2f-a): `LinkMachine` without suspend, IDF worker, `WifiLink`, 7 hal examples converted, `idf_esp32s3_mqtt_lwt`, publish parity
- [x] PR 2 (U2f-b): suspend inputs, `suspend()`/`resume()` on both tiers, demo flag, deep-sleep docs
- [x] Tests passing
- [x] Documentation updated

Hardware: covered by the release 0.7.0 hardware run, see ../runbooks/release-0.7.0-hardware-test.md

## Session Log
- 2026-10-31 — Feature doc created from request U2f after design review; reconnect policy moved into one pure machine at the maintainer's request
- 2026-10-31 — PR 1 implemented on `reconnect-rework`: `LinkMachine` (8 host tests), IDF worker, `WifiLink` + `link_task`, 7 hal examples converted, `idf_esp32s3_mqtt_lwt`, IDF `try_publish*` gate, bug 009, runbook `wifi-reconnect-hardware-test.md`; all 8 example builds green; hardware gate open
- 2026-10-31 — PR 1 review applied: IDF first connect goes through the worker in every mode, `LinkWorker` joins before the driver drops, hal `run` resets on entry and counts an adopted link, machine adapter contract documented
- 2026-11-01 — PR 2 implemented: suspend/resume on both tiers, IDF connect_count, MQTT_SUSPEND_DEMO, deep-sleep guide docs/wifi-suspend-and-deep-sleep.md, runbook R3; 14 machine tests; hardware gate open
- 2026-11-01 — PR 2 review applied: hal handover published under the state lock, IDF stop fence moved into resume and the worker, adapters extracted to juggler `LinkHandover` / `LinkSupervisor` with host tests
- 2026-11-01 — Second and third review applied: no fence timeout in resume (a lost `Stopped` expires after 5 s in the worker), back-off deadline kept under the core lock, failure-injection concurrency tests, radio difference between tiers made prominent
- 2026-11-01 — Fourth review applied: the stop fence has no timeout, only `Stopped` re-admits link events; the in-order, no-loss event contract is documented and late-event tests cover delays beyond every timer
- 2026-11-02 — R3 suspend/resume run on the CrowPanel S3 for both tiers, 3 cycles each, pass; `suspend` now reports readiness before the radio stops (removes a spurious MQTT retry, keeps one teardown error on IDF); R1 still needs a router pause
- 2026-11-02 — R1 router pause run on the CrowPanel S3 (ESP-IDF): back-off 1000→30000 ms never higher, ticks `would block`, one teardown error, MQTT back 270 ms after `sta ip:`, no reboot; esp-hal R1 not run
- 2026-11-11 — R3 on the bench for both tiers (`just bench s3 suspend` 13/13 each, three cycles, `wait_suspended` prompt, no reconnect while suspended, one connect per resume, will and `online` at the broker); the attended R1 run on both tiers timed out at the pause step with ticks still queued, so the esp-hal R1 cell and the IDF bench R1 stayed open
- 2026-11-11 — esp-hal R1 on the bench (`just bench s3 mqtt-scenarios --attended` 10/10, `hal_esp32s3_mqtt_lwt`): `AuthenticationLeave` on the pause, back-off 1000→30000 ms never higher, ticks `would block`, will at the broker, `link up #2` and one connect after the unpause, no reboot; the esp-hal R1 cell is closed, the IDF bench R1 run follows
- 2026-11-11 — IDF R1 on the bench too (`just bench s3 mqtt-scenarios --attended` 10/10, `idf_esp32s3_mqtt_lwt`): `AUTH_LEAVE` on the pause, one `ESP_FAIL` MQTT error, back-off capped at 30000 ms, will at the broker, `link up #2` after the unpause, no reboot; the state gate (R1 and R3, both tiers) is closed on the bench
